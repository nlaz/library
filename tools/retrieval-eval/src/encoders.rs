//! Pluggable encoders for the encoder-level eval. Compile-time ese variants
//! (quantization, dimension) are NOT runtime plugins — rebuild with edited
//! workspace features and compare labeled JSON outputs (see README).

use anyhow::{Context, Result, bail};
use model2vec_rs::model::StaticModel;

pub trait Encoder {
    fn name(&self) -> String;
    fn encode(&self, texts: &[String]) -> Vec<Vec<f32>>;
}

/// The production encoder: the baked-in static model behind the app.
pub struct Ese;

impl Encoder for Ese {
    fn name(&self) -> String {
        format!("ese(dim={})", ese::DIMENSIONS)
    }
    fn encode(&self, texts: &[String]) -> Vec<Vec<f32>> {
        ese::encode(texts).into_iter().map(|e| e.to_vec()).collect()
    }
}

/// model2vec baseline, e.g. minishlab/potion-retrieval-32M. Downloads from
/// the HF hub on first use.
pub struct Potion {
    model: StaticModel,
    id: String,
}

impl Encoder for Potion {
    fn name(&self) -> String {
        self.id.clone()
    }
    fn encode(&self, texts: &[String]) -> Vec<Vec<f32>> {
        self.model.encode(texts)
    }
}

/// `"ese"` or `"potion:<hf-hub-id>"`.
pub fn make(spec: &str) -> Result<Box<dyn Encoder>> {
    match spec.split_once(':') {
        None if spec == "ese" => Ok(Box::new(Ese)),
        Some(("potion", id)) => {
            let model = StaticModel::from_pretrained(id, None, None, None)
                .with_context(|| format!("load model2vec model {id}"))?;
            Ok(Box::new(Potion {
                model,
                id: id.to_string(),
            }))
        }
        _ => bail!("unknown encoder {spec:?} (want ese | potion:<hf-id>)"),
    }
}
