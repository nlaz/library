//! Document layout detection: YOLOv10m trained on DocLayNet, run via ort.
//!
//! The ONNX export is NMS-free — the model emits up to 300 final boxes as
//! [xmin, ymin, xmax, ymax, score, class_id] in input-pixel (640) space, so
//! postprocessing is a score threshold plus coordinate un-mapping.
//!
//! Model file (not vendored): data/models/layout/yolov10m-doclaynet.onnx from
//! huggingface.co/Oblix/yolov10m-doclaynet_ONNX_document-layout-analysis. The
//! model is AGPL-3.0; it is fetched at runtime by your own build, not shipped
//! here. Review its terms before redistributing it or serving it over a
//! network. When the file is absent, ingestion falls back to the word-gap
//! heuristic below, so this dependency is optional. See NOTICE.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use image::{DynamicImage, GenericImageView, imageops::FilterType};
use library_core::Bbox;
use ndarray::{Array4, Axis};
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::Value;

/// Detections below this score are dropped.
pub const SCORE_MIN: f32 = 0.35;
/// Figure candidates smaller than this fraction of the page are dropped.
pub const AREA_MIN: f32 = 0.01;
const INPUT: u32 = 640;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Caption,
    Footnote,
    Formula,
    ListItem,
    PageFooter,
    PageHeader,
    Picture,
    SectionHeader,
    Table,
    Text,
    Title,
}

impl Class {
    fn from_id(id: usize) -> Option<Class> {
        use Class::*;
        [
            Caption,
            Footnote,
            Formula,
            ListItem,
            PageFooter,
            PageHeader,
            Picture,
            SectionHeader,
            Table,
            Text,
            Title,
        ]
        .get(id)
        .copied()
    }

    pub fn name(self) -> &'static str {
        match self {
            Class::Caption => "caption",
            Class::Footnote => "footnote",
            Class::Formula => "formula",
            Class::ListItem => "list-item",
            Class::PageFooter => "page-footer",
            Class::PageHeader => "page-header",
            Class::Picture => "picture",
            Class::SectionHeader => "section-header",
            Class::Table => "table",
            Class::Text => "text",
            Class::Title => "title",
        }
    }

    /// The classes we index as searchable figures.
    pub fn is_figure(self) -> bool {
        matches!(self, Class::Picture | Class::Table | Class::Formula)
    }
}

#[derive(Debug, Clone)]
pub struct Detection {
    pub class: Class,
    pub score: f32,
    /// Normalized [x, y, w, h], top-left origin — same space as OCR boxes.
    pub bbox: Bbox,
}

pub struct LayoutModel {
    session: Session,
}

impl LayoutModel {
    pub fn model_path(data: &Path) -> PathBuf {
        data.join("models")
            .join("layout")
            .join("yolov10m-doclaynet.onnx")
    }

    /// Ok(None) when the model file is absent — caller falls back to the
    /// word-gap heuristic.
    pub fn load(data: &Path) -> Result<Option<Self>> {
        let path = Self::model_path(data);
        if !path.exists() {
            return Ok(None);
        }
        let session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_intra_threads(2)?
            .commit_from_file(&path)
            .with_context(|| format!("loading layout model {}", path.display()))?;
        Ok(Some(LayoutModel { session }))
    }

    /// All detections on a page render, score-descending, in normalized
    /// page coordinates.
    pub fn detect(&self, page: &DynamicImage) -> Result<Vec<Detection>> {
        // resize longest edge to 640, pad bottom/right with gray — top-left
        // anchoring means un-mapping is a plain divide by scale
        let (pw, ph) = page.dimensions();
        let scale = INPUT as f32 / pw.max(ph) as f32;
        let rw = ((pw as f32 * scale) as u32).clamp(1, INPUT);
        let rh = ((ph as f32 * scale) as u32).clamp(1, INPUT);
        let resized = page.resize_exact(rw, rh, FilterType::Triangle).into_rgb8();

        let mut input =
            Array4::<f32>::from_elem((1, 3, INPUT as usize, INPUT as usize), 114.0 / 255.0);
        for (x, y, px) in resized.enumerate_pixels() {
            for c in 0..3 {
                input[[0, c, y as usize, x as usize]] = px.0[c] as f32 / 255.0;
            }
        }

        let input_name = self.session.inputs[0].name.clone();
        let outputs = self
            .session
            .run(ort::inputs![input_name => Value::from_array(input)?]?)?;
        let key = outputs
            .keys()
            .next()
            .context("layout model produced no outputs")?;
        let out = outputs
            .get(key)
            .expect("key just came from outputs.keys()")
            .try_extract_tensor::<f32>()
            .context("layout output is not an f32 tensor")?;

        // [1, 300, 6] -> [300, 6]
        anyhow::ensure!(
            out.ndim() == 3 && out.shape()[2] == 6,
            "unexpected output shape {:?}",
            out.shape()
        );
        let rows = out.index_axis(Axis(0), 0);

        let mut dets = Vec::new();
        for row in rows.outer_iter() {
            let (x0, y0, x1, y1, score, id) = (row[0], row[1], row[2], row[3], row[4], row[5]);
            if score < SCORE_MIN {
                continue;
            }
            let Some(class) = Class::from_id(id as usize) else {
                continue;
            };
            let bx = (x0 / scale / pw as f32).clamp(0.0, 1.0);
            let by = (y0 / scale / ph as f32).clamp(0.0, 1.0);
            let bw = ((x1 - x0) / scale / pw as f32).clamp(0.0, 1.0 - bx);
            let bh = ((y1 - y0) / scale / ph as f32).clamp(0.0, 1.0 - by);
            dets.push(Detection {
                class,
                score,
                bbox: [bx, by, bw, bh],
            });
        }
        Ok(dets)
    }
}
