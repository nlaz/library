// ---------------------------------------------------------------------------
// hero.js — the ASCII shader behind the masthead.
//
// A signed-distance model of a vaulted library nave, raymarched in WebGL2 at
// character resolution and resolved through a glyph atlas measured at runtime
// from IBM Plex Mono. The camera dollies down the nave at a hundred seconds a
// bay and wraps on that bay; because the room is periodic in depth, the wrap
// lands on an identical frame and the walk is seamless. No dissolve, no seam,
// no video file.
//
// Two passes: the scene renders into a small framebuffer sized to the
// character grid (a 1440x900 viewport is only ~360x128 cells, so the marching
// is nearly free), then a fullscreen pass turns each cell into a glyph and
// tints it. Glyph choice comes from luminance; tint comes from luminance at
// gamma, so the accent tracks how lit a surface is rather than smearing over
// the whole frame.
//
// The tint is the "lamplight" reading of the palette: --hl and --hl-line are
// the dark-theme values from apps/web/src/styles/tokens.css §2 exactly, and
// the two stops below them are the neutral tokens warmed toward that same
// accent, so the room is lit by one colour instead of being a grey room with
// gold on it. Still one accent — it just arrives a third of the way up the
// range instead of only at the top.
//
// If anything is missing — no WebGL2, no webfont — the page keeps its ground
// colour and the masthead reads normally. The hero is decoration.
// ---------------------------------------------------------------------------

const CELL = { w: 4, h: 7 };          // CSS px per character cell
const GLYPHS = 12;                    // ramp levels
const MAX_COLS = 640, MAX_ROWS = 288;
const FPS = 24;                       // ASCII at 60fps reads as jitter: every
const FRAME_MS = 1000 / FPS;          //   frame is a hard cut when quantized

// Biased to verticals and horizontals — architecture is orthogonal, and
// diagonal-heavy glyphs read as noise in the mid-tones. No block elements:
// the woff2 is a Latin subset and a missing glyph would silently fall back to
// a proportional font, breaking the grid.
const POOL = " .'`,:;-~_=+ilrtcvxznuoseCLEZS0OQGD#%8B&@$";

// Exposure and lighting. black/white are the ends of the window the scene's
// luminance is mapped through, and their SPREAD is the contrast control: the
// wider the window, the fewer cells clip at either end and the flatter the
// result. Deliberately wider than the measured histogram here, so the hero
// stays quiet behind the type.
const SCENE = {
  tan: Math.tan((50 * 0.5 * Math.PI) / 180),
  far: 46, fog: 0.016, black: 0.240, white: 0.800,
  key: [0.38, 0.72, -0.30], head: 0.44,
  steps: 180, scale: 0.88, shadow: 0,
  pose: 12.83,                        // the frame reduced-motion rests on —
                                      //   in seconds, so it tracks dolly speed
};

const FONT = '"IBM Plex Mono", ui-monospace, monospace';
const reduced = matchMedia("(prefers-reduced-motion: reduce)");

const VERT = `#version 300 es
// fullscreen triangle, no VBO — the vertex is derived from gl_VertexID
out vec2 v_uv;
void main() {
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  v_uv = p;
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;

const ASCII_PASS = `#version 300 es
precision highp float;

uniform sampler2D uScene;    // RGBA8: R lum, G depth, B material, A rim
uniform sampler2D uAtlas;    // RGBA8, single row of glyphs, white on clear
uniform vec2  uGrid;
uniform vec2  uAtlasSize;
uniform float uAtlasStride;  // texels per glyph cell, gutter included
uniform float uAtlasInkW;    // ink width inside the stride
uniform int   uGlyphs;

in  vec2 v_uv;
out vec4 fragColor;

// The ground is --bg taken down and warmed; site/style.css carries the same
// value as --ground and the two MUST agree, or the sliver of page under the
// canvas bands against the scene.
//
// The two lower stops are NOT the neutral tokens: they are --line and
// --ink-dim rotated toward the accent's hue and taken down a little, so the
// stone reads as lit by the same brass the highlights are rather than sitting
// neutral underneath it. The top two stops are the tokens exactly.
const vec3 BG = vec3(0.05882, 0.04314, 0.02745); // #0f0b07  ground
const vec3 C0 = vec3(0.16471, 0.12549, 0.08627); // #2a2016  --line, warmed
const vec3 C1 = vec3(0.54118, 0.45098, 0.34510); // #8a7358  --ink-dim, warmed
const vec3 C2 = vec3(0.85490, 0.68627, 0.41961); // #daaf6b  --hl
const vec3 C3 = vec3(0.92157, 0.76471, 0.52157); // #ebc385  --hl-line

// Still one accent, but it arrives a third of the way up rather than at the
// top: the bottom 42% is stone, the turn into brass runs 42-80%, and --hl-line
// shows over the top 20%. Lamplight rather than a highlighter — the room is
// lit, not merely visible.
vec3 brass(float t){
  t = clamp(t, 0.0, 1.0);
  vec3 c = mix(C0, C1, smoothstep(0.00, 0.42, t));
  c = mix(c, C2, smoothstep(0.42, 0.80, t));
  c = mix(c, C3, smoothstep(0.80, 1.00, t));
  return c;
}

// the same 4x4 Bayer threshold order as the app's --dither-* tiles, applied
// at CELL granularity — the house texture primitive, expressed typographically
float bayer4(ivec2 c){
  int m[16] = int[16](0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5);
  int i = (c.y & 3) * 4 + (c.x & 3);
  return float(m[i]) / 16.0;
}

void main(){
  vec2 cellUV = v_uv * uGrid;
  ivec2 cid = ivec2(floor(cellUV));
  vec2 inCell = fract(cellUV);
  inCell.y = 1.0 - inCell.y;              // the atlas is rasterized top-down

  vec4 s = texelFetch(uScene, cid, 0);
  float lum = s.r, depth = s.g, rim = s.a;

  // glyph from luminance (+ rim, so silhouettes keep a heavy glyph even when
  // facing away from the key) — form
  float g = clamp(lum + rim * 0.22, 0.0, 1.0);
  // half a level of ordered dither, not a full one: at full amplitude a flat
  // wall speckles one cell at a time, and isolated glyphs read as noise
  g += (bayer4(cid) - 0.5) * (0.5 / float(uGlyphs));
  int gi = int(clamp(floor(g * float(uGlyphs)), 0.0, float(uGlyphs - 1)));

  // atlas lookup: half-texel inset inside a 1px transparent gutter
  vec2 texel = 1.0 / uAtlasSize;
  float x0 = (float(gi) * uAtlasStride + 1.0) / uAtlasSize.x;
  float w  = uAtlasInkW / uAtlasSize.x;
  vec2 inset = 0.5 * texel;
  vec2 auv = vec2(x0 + inset.x + inCell.x * (w - 2.0 * inset.x),
                  texel.y + inset.y + inCell.y * (1.0 - 2.0 * texel.y - 2.0 * inset.y));
  float ink = texture(uAtlas, auv).a;

  // Colour from luminance modulated by depth — quantizing both on the same
  // signal makes the image read as flat plateaus, like a chart.
  //
  // The gamma sets how late the accent arrives. Glyph selection wants the full
  // range so structure reads, but tint must not: at gamma 2.2 a mid-tone of
  // 0.6 lands at 0.33 (warm stone) while 0.85 lands at 0.69 (well into brass),
  // so the lit half of the room takes the accent and the shadowed half does
  // not. The depth term then pulls colour back out with distance, so the far
  // end of the nave stays stone however bright it gets.
  float hueT = pow(lum, 2.2) * (1.0 - 0.36 * depth);

  // DIM pulls the whole field back toward the ground. The hero sits behind
  // type, so it wants to read as texture at the edge of vision rather than as
  // an image competing with the masthead — and a warm palette needs more of
  // that restraint than a neutral one did.
  const float DIM = 0.64;
  fragColor = vec4(mix(BG, brass(hueT), ink * DIM), 1.0);
}`;

const PRELUDE = `#version 300 es
precision highp float;

uniform float uTime;
uniform vec2  uGrid;       // cols, rows — the render target IS the char grid
uniform float uAspect;     // (cols*cellW)/(rows*cellH)
uniform float uTanHalf;    // tan(vertical fov / 2)
uniform float uFar;
uniform float uFog;
uniform float uBlack;      // per-scene exposure — no auto-exposure works
uniform float uWhite;      //   across a barrel vault and an astrolabe
uniform vec3  uKey;
uniform float uHead;       // headlight weight: interiors face away from any
                           //   fixed key, so form has to come off the camera
uniform float uStepScale;
uniform float uParam;      // per-scene knob (flute count, mostly)
uniform int   uSteps;
uniform int   uShadow;

in  vec2 v_uv;
out vec4 fragColor;

#define PI  3.14159265359
#define TAU 6.28318530718

// distance along the primary ray at the current map() call — read by
// minThick() so sub-cell members can be fattened with depth
float gT = 0.0;

// --- rotations -------------------------------------------------------------
mat3 rotX(float a){ float s=sin(a),c=cos(a); return mat3(1.,0.,0., 0.,c,s, 0.,-s,c); }
mat3 rotY(float a){ float s=sin(a),c=cos(a); return mat3(c,0.,-s, 0.,1.,0., s,0.,c); }
mat3 rotZ(float a){ float s=sin(a),c=cos(a); return mat3(c,s,0., -s,c,0., 0.,0.,1.); }

// --- primitives ------------------------------------------------------------
float sdSphere(vec3 p, float r){ return length(p) - r; }

float sdBox(vec3 p, vec3 b){
  vec3 q = abs(p) - b;
  return length(max(q, 0.0)) + min(max(q.x, max(q.y, q.z)), 0.0);
}

float sdBox2(vec2 p, vec2 b){
  vec2 q = abs(p) - b;
  return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0);
}

float sdCyl(vec3 p, float h, float r){
  vec2 d = abs(vec2(length(p.xz), p.y)) - vec2(r, h);
  return min(max(d.x, d.y), 0.0) + length(max(d, 0.0));
}

float sdTorus(vec3 p, vec2 t){
  return length(vec2(length(p.xz) - t.x, p.y)) - t.y;
}

float sdEllipsoid(vec3 p, vec3 r){
  float k0 = length(p / r);
  float k1 = length(p / (r * r));
  return k0 * (k0 - 1.0) / k1;
}

float sdCapsule(vec3 p, vec3 a, vec3 b, float r){
  vec3 pa = p - a, ba = b - a;
  float h = clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0);
  return length(pa - ba * h) - r;
}

float sdCone(vec3 p, float h, float r1, float r2){
  vec2 q = vec2(length(p.xz), p.y);
  vec2 k1 = vec2(r2, h);
  vec2 k2 = vec2(r2 - r1, 2.0 * h);
  vec2 ca = vec2(q.x - min(q.x, (q.y < 0.0) ? r1 : r2), abs(q.y) - h);
  vec2 cb = q - k1 + k2 * clamp(dot(k1 - q, k2) / dot(k2, k2), 0.0, 1.0);
  float s = (cb.x < 0.0 && ca.y < 0.0) ? -1.0 : 1.0;
  return s * sqrt(min(dot(ca, ca), dot(cb, cb)));
}

// regular n-gon, apothem r. Underestimates near corners, which is the safe
// direction for a marcher.
float sdNgon(vec2 p, float r, float n){
  float seg = TAU / n;
  float a = mod(atan(p.y, p.x) + 0.5 * seg, seg) - 0.5 * seg;
  return length(p) * cos(a) - r;
}

// --- operators -------------------------------------------------------------
vec2  opU(vec2 a, vec2 b){ return a.x < b.x ? a : b; }
float opOnion(float d, float t){ return abs(d) - t; }
float smin(float a, float b, float k){
  float h = clamp(0.5 + 0.5 * (b - a) / k, 0.0, 1.0);
  return mix(b, a, h) - k * h * (1.0 - h);
}

// limited repetition only — infinite mod() repetition destroys the far-field
// bound and interiors then march to the step cap
float repLim1(float p, float c, float l){
  return p - c * clamp(round(p / c), -l, l);
}

// angular fold. NOT distance-preserving: scenes using it need uStepScale ~0.7
vec2 pmod2(vec2 p, float n){
  float seg = TAU / n;
  float a = mod(atan(p.y, p.x) + 0.5 * seg, seg) - 0.5 * seg;
  return vec2(cos(a), sin(a)) * length(p);
}

// --- cell metrics ----------------------------------------------------------
// world-space size of one character cell at ray distance t. Anything thinner
// than ~1 cell strobes along its length, which reads as a bug rather than a
// style, so thin members grow with depth.
float cellWorld(float t){ return t * uTanHalf * 2.0 / uGrid.y; }
float minThick(float t, float r){ return max(r, 0.55 * cellWorld(t)); }

// --- cameras ---------------------------------------------------------------
void lookAtUp(vec2 uv, vec3 o, vec3 ta, vec3 up, out vec3 ro, out vec3 rd){
  vec3 f = normalize(ta - o);
  vec3 r = normalize(cross(up, f));
  vec3 u = cross(f, r);
  ro = o;
  rd = normalize(f + uTanHalf * (uv.x * r + uv.y * u));
}
void lookAt(vec2 uv, vec3 o, vec3 ta, out vec3 ro, out vec3 rd){
  lookAtUp(uv, o, ta, vec3(0.0, 1.0, 0.0), ro, rd);
}

// --- shared shading helpers ------------------------------------------------
// a lit band with a hard edge — used for book spines, coffer lips, graduations
float stripe(float x, float period, float duty){
  return step(fract(x / period), duty);
}`;

const NAVE = `#define BAY 2.30

// materials: 1 stone · 2 oak · 3 books · 4 light · 5 joinery/iron
//            6 floor · 7 vault
float naveShell(vec3 p, float bz){
  vec2 vq = vec2(p.x, p.y - 6.30);
  float vault = opOnion(length(vq) - 3.60, 0.18);
  vault = max(vault, 6.30 - p.y);
  float rib = opOnion(length(vq) - 3.72, 0.14);
  rib = max(rib, 6.30 - p.y);
  rib = max(rib, abs(bz) - minThick(gT, 0.12));
  float ang = atan(p.x, max(p.y - 6.30, 1e-4));
  float lrib = opOnion(length(vq) - 3.70, 0.10);
  lrib = max(lrib, 6.30 - p.y);
  lrib = max(lrib, abs(abs(ang) - 0.66) * 3.60 - minThick(gT, 0.075));
  return min(vault, min(rib, lrib));
}

// vault, floor and the bounce term — shared, so the three variants differ
// only in what they add on top
float naveShade(int id, vec3 p, vec3 n, float lum){
  if (id == 7){
    float a = atan(p.x, max(p.y - 6.30, 1e-4));
    lum *= 0.88 + 0.18 * smoothstep(0.30, 0.46,
             min(abs(fract(a / 0.33) - 0.5),
                 abs(fract(p.z / (BAY * 0.5)) - 0.5)));
    lum += 0.20;                       // bounce-lit only; needs a pedestal
  }
  if (id == 6) lum *= 1.55 * (0.86 + 0.22 * stripe(p.x, 0.26, 0.5));
  lum += 0.19 * max(-n.y, 0.0);
  return lum;
}

// the axial dolly. Speed only sets how long a cycle takes — the wrap is on
// BAY, so any speed loops exactly and this can be tuned purely by feel.
//
// It is tuned by the FASTEST thing on screen, not the average. Forward motion
// makes the optical flow radial from the vanishing point, so it is slowest at
// the centre and fastest in the corners — where the lit vertical edges of the
// standards and window reveals live, streaming up and down the frame edges at
// several times the rate anything in the middle moves. Set the speed so THAT
// reads as a drift and the centre of the frame will look nearly still.
void camera(vec2 uv, out vec3 ro, out vec3 rd){
  float z = mod(uTime * 0.023, BAY);        // 100s a bay
  vec3 o = vec3(0.0, 1.72, -8.0 + z);
  lookAt(uv, o, o + vec3(0.0, 0.30, 6.0), ro, rd);
}
vec3 matNormal(int id, vec3 p, vec3 n){ return n; }`;

const SIDES = `// I · OPEN TIERS — the arcade taken away.
// No wall at the nave face at all: three storeys of shelving stand open to
// the walk, tied back by slender iron balconies, and the alcove window runs
// the full height behind them. The tallest and airiest of the three, and the
// only one where you can see clean through the sides.
vec2 map(vec3 p){
  vec2 r = vec2(p.y, 6.0);
  float bz = repLim1(p.z, BAY, 26.0);
  float az = repLim1(p.z + BAY * 0.5, BAY, 26.0);
  vec3 s = p; s.x = abs(s.x);
  r = opU(r, vec2(naveShell(p, bz), 7.0));

  float t1 = sdBox(vec3(s.x - 5.05, p.y - 1.16, bz), vec3(1.62, 1.16, 0.58));
  float t2 = sdBox(vec3(s.x - 5.05, p.y - 3.38, bz), vec3(1.62, 0.92, 0.58));
  float t3 = sdBox(vec3(s.x - 5.05, p.y - 5.34, bz), vec3(1.62, 0.80, 0.58));
  r = opU(r, vec2(min(t1, min(t2, t3)), 2.0));

  float clip = min(min(sdBox(vec3(s.x - 5.05, p.y - 1.16, bz), vec3(1.62, 1.10, 0.78)),
                       sdBox(vec3(s.x - 5.05, p.y - 3.38, bz), vec3(1.62, 0.86, 0.78))),
                       sdBox(vec3(s.x - 5.05, p.y - 5.34, bz), vec3(1.62, 0.74, 0.78)));
  float sy = repLim1(p.y - 0.34, 0.56, 11.0);
  r = opU(r, vec2(max(sdBox(vec3(s.x - 5.05, sy, bz), vec3(1.54, 0.21, 0.64)),
                      clip), 3.0));

  // iron balconies tying the tiers back to the nave
  for (int i = 0; i < 2; i++){
    float y0 = 2.44 + 2.02 * float(i);
    float walk = sdBox(vec3(s.x - 3.86, p.y - y0, p.z), vec3(0.62, 0.05, 58.0));
    float qz = repLim1(p.z, BAY / 8.0, 200.0);
    float balu = sdBox(vec3(s.x - 3.28, p.y - (y0 + 0.34), qz),
                       vec3(0.045, 0.34, minThick(gT, 0.042)));
    float hand = sdBox(vec3(s.x - 3.28, p.y - (y0 + 0.72), p.z),
                       vec3(0.11, 0.055, 58.0));
    r = opU(r, vec2(min(walk, min(balu, hand)), 5.0));
  }

  // standards at the nave end of each range, full height
  float std = sdBox(vec3(s.x - 3.46, p.y - 3.10, bz), vec3(0.13, 3.10, 0.62));
  r = opU(r, vec2(std, 5.0));

  // the full-height alcove window
  float wy = p.y - 3.30;
  float win = length(vec2(az * 1.22, max(wy, 0.0))) - 1.00;
  win = max(win, -wy - 2.95);
  r = opU(r, vec2(max(win, abs(s.x - 6.70) - 0.05), 4.0));
  float wall = sdBox(vec3(s.x - 6.92, p.y - 3.30, p.z), vec3(0.26, 3.30, 58.0));
  r = opU(r, vec2(max(wall, -max(win, abs(s.x - 6.92) - 0.50)), 1.0));
  return r;
}
float matShade(int id, vec3 p, vec3 n, float lum){
  if (id == 4) return 1.42;
  if (id == 3) lum *= 0.72 + 0.34 * stripe(p.x, 0.072, 0.55)
                           + 0.12 * stripe(p.x, 0.031, 0.50);
  if (id == 2) lum *= 0.90;
  if (id == 5) lum *= 1.12;
  return naveShade(id, p, n, lum);
}`;

const EPILOGUE = `vec3 calcNormal(vec3 p){
  const vec2 k = vec2(1.0, -1.0);
  const float h = 0.0018;
  return normalize(k.xyy * map(p + k.xyy * h).x + k.yyx * map(p + k.yyx * h).x
                 + k.yxy * map(p + k.yxy * h).x + k.xxx * map(p + k.xxx * h).x);
}

// 5-tap Quilez AO. Step-count AO is free but its character changes with camera
// distance and step scale, so it drifts between scenes and flickers.
float calcAO(vec3 p, vec3 n, float sc){
  float occ = 0.0, sca = 1.0;
  for (int i = 0; i < 5; i++){
    float h = (0.012 + 0.085 * float(i) / 4.0) * sc;
    occ += (h - map(p + n * h).x) * sca;
    sca *= 0.86;
  }
  return clamp(1.0 - 2.4 * occ, 0.0, 1.0);
}

float calcShadow(vec3 ro, vec3 rd, float mint, float maxt){
  float res = 1.0, t = mint;
  for (int i = 0; i < 28; i++){
    float h = map(ro + rd * t).x;
    res = min(res, 10.0 * h / t);
    t += clamp(h, 0.02, 0.4);
    if (res < 0.005 || t > maxt) break;
  }
  return clamp(res, 0.0, 1.0);
}

void main(){
  vec2 uv = v_uv * 2.0 - 1.0;
  uv.x *= uAspect;

  vec3 ro, rd;
  camera(uv, ro, rd);

  float t = 0.02, m = -1.0;
  for (int i = 0; i < 220; i++){
    if (i >= uSteps) break;
    gT = t;
    vec2 h = map(ro + rd * t);
    if (h.x < 0.0009 * t){ m = h.y; break; }
    t += h.x * uStepScale;
    if (t > uFar) break;
  }

  float lum = 0.0, depth = 1.0, rim = 0.0, mid = 0.0;
  if (m > -0.5){
    vec3 p = ro + rd * t;
    vec3 n = matNormal(int(m), p, calcNormal(p));
    vec3 L = normalize(uKey);

    // wrap lighting, not Lambert — Lambert crushes half of every frame to
    // zero and throws away half of twelve levels
    const float w = 0.35;
    float key = clamp((dot(n, L) + w) / (1.0 + w), 0.0, 1.0);
    if (uShadow == 1) key *= calcShadow(p + n * 0.03, L, 0.03, uFar * 0.6);

    float fill = 0.5 + 0.5 * n.y;
    float head = clamp(dot(n, -rd), 0.0, 1.0);
    float spec = pow(max(dot(n, normalize(L - rd)), 0.0), 26.0);
    float ao = calcAO(p, n, 1.0);

    lum = (key * (0.70 - uHead) + head * uHead + fill * 0.24 + spec * 0.06) * ao;
    lum = matShade(int(m), p, n, lum);
    // material 6 is the floor in every scene that has one. Floors face the
    // sky, so they take the full fill term and blow out unless held down —
    // and a dark floor is what lets the walls and vaults read.
    if (int(m) == 6) lum *= 0.42;
    lum *= exp(-uFog * t);
    // uWhite <= uBlack is the calibration sentinel: emit raw luminance so the
    // levels can be measured rather than guessed
    lum = (uWhite > uBlack) ? smoothstep(uBlack, uWhite, lum) : clamp(lum, 0.0, 1.0);

    depth = clamp(t / uFar, 0.0, 1.0);
    rim = pow(1.0 - max(dot(n, -rd), 0.0), 3.0);
    mid = m / 255.0;
  }
  fragColor = vec4(lum, depth, mid, rim);
}`;

// --- gl bootstrap ----------------------------------------------------------

const canvas = document.getElementById("hero");
const gl = canvas && canvas.getContext("webgl2", {
  alpha: false, depth: false, stencil: false, antialias: false,
  preserveDrawingBuffer: false, powerPreference: "low-power",
});

let sceneTex, fbo, atlasTex, sceneProg, asciiProg, atlas = null;
let cols = 0, rows = 0, dpr = 1;
let raf = 0, last = 0, t0 = 0, onScreen = true;

function shader(type, src) {
  const s = gl.createShader(type);
  gl.shaderSource(s, src);
  gl.compileShader(s);
  return s;
}

function program(fsSrc, label) {
  const p = gl.createProgram();
  gl.attachShader(p, shader(gl.VERTEX_SHADER, VERT));
  const fs = shader(gl.FRAGMENT_SHADER, fsSrc);
  gl.attachShader(p, fs);
  gl.linkProgram(p);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) {
    console.warn("hero: " + label + " failed to link", gl.getShaderInfoLog(fs));
    return null;
  }
  return p;
}

function uniforms(p, names) {
  const u = {};
  for (const n of names) u[n] = gl.getUniformLocation(p, n);
  return u;
}

function glInit() {
  gl.bindVertexArray(gl.createVertexArray());  // some drivers want one bound

  sceneTex = gl.createTexture();
  gl.bindTexture(gl.TEXTURE_2D, sceneTex);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, MAX_COLS, MAX_ROWS, 0,
                gl.RGBA, gl.UNSIGNED_BYTE, null);
  for (const k of ["TEXTURE_MIN_FILTER", "TEXTURE_MAG_FILTER"])
    gl.texParameteri(gl.TEXTURE_2D, gl[k], gl.NEAREST);
  for (const k of ["TEXTURE_WRAP_S", "TEXTURE_WRAP_T"])
    gl.texParameteri(gl.TEXTURE_2D, gl[k], gl.CLAMP_TO_EDGE);

  fbo = gl.createFramebuffer();
  gl.bindFramebuffer(gl.FRAMEBUFFER, fbo);
  gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0,
                          gl.TEXTURE_2D, sceneTex, 0);
  gl.bindFramebuffer(gl.FRAMEBUFFER, null);

  sceneProg = program(PRELUDE + "\n" + NAVE + "\n" + SIDES + "\n" + EPILOGUE, "scene");
  asciiProg = program(ASCII_PASS, "ascii");
  if (!sceneProg || !asciiProg) return false;

  sceneProg.uni = uniforms(sceneProg,
    ["uTime", "uGrid", "uAspect", "uTanHalf", "uFar", "uFog", "uBlack",
     "uWhite", "uKey", "uHead", "uStepScale", "uParam", "uSteps", "uShadow"]);
  asciiProg.uni = uniforms(asciiProg,
    ["uScene", "uAtlas", "uGrid", "uAtlasSize", "uAtlasStride", "uAtlasInkW",
     "uGlyphs"]);
  return true;
}

// --- glyph atlas -----------------------------------------------------------
// Rasterized at exactly the device cell size and sampled NEAREST, so glyphs
// stay pixel-exact and hard-edged. The ramp is measured, not hardcoded: raw
// ASCII ramps cluster a dozen glyphs between 0.10 and 0.30 ink coverage and
// then chasm up to '@'.

async function ensureFont(px) {
  // document.fonts.ready alone is not enough — a @font-face font is only
  // fetched when something asks for it, so ready() can resolve having never
  // requested it, and the atlas silently falls back to the system monospace.
  try { await document.fonts.load(`400 ${px}px "IBM Plex Mono"`); } catch (e) {}
  try { await document.fonts.ready; } catch (e) {}
}

function buildAtlas(cw, ch, px) {
  const scratch = document.createElement("canvas");
  scratch.width = cw; scratch.height = ch;
  const sc = scratch.getContext("2d", { willReadFrequently: true });
  sc.font = `400 ${px}px ${FONT}`;
  sc.textAlign = "center";

  const ref = sc.measureText("M").width;
  const m = sc.measureText("MOX");
  const baseline = (ch + (m.actualBoundingBoxAscent - m.actualBoundingBoxDescent)) / 2;

  // an advance-width check catches the font-not-loaded case, a missing subset
  // glyph, and any future font swap, all at once
  const cand = [];
  for (const g of POOL) {
    if (Math.abs(sc.measureText(g).width - ref) > 0.01) continue;
    sc.clearRect(0, 0, cw, ch);
    sc.fillStyle = "#fff";
    sc.fillText(g, cw / 2, baseline);
    const d = sc.getImageData(0, 0, cw, ch).data;
    let sum = 0;
    for (let i = 3; i < d.length; i += 4) sum += d[i];
    cand.push({ g, cov: sum / (255 * cw * ch) });
  }
  if (!cand.length) return null;
  cand.sort((a, b) => a.cov - b.cov);

  const max = cand[cand.length - 1].cov || 1;
  const used = new Set(), ramp = [];
  for (let k = 0; k < GLYPHS; k++) {
    const target = (k / (GLYPHS - 1)) * max;
    let best = -1, bd = 1e9;
    for (let i = 0; i < cand.length; i++) {
      if (used.has(i)) continue;
      const d = Math.abs(cand[i].cov - target);
      if (d < bd) { bd = d; best = i; }
    }
    used.add(best);
    ramp.push(cand[best]);
  }
  ramp[0] = { g: " ", cov: 0 };       // level 0 is the ground, always

  const stride = cw + 2;              // 1px transparent gutter, both sides
  const cv = document.createElement("canvas");
  cv.width = stride * GLYPHS; cv.height = ch + 2;
  const ctx = cv.getContext("2d");
  ctx.font = `400 ${px}px ${FONT}`;
  ctx.textAlign = "center";
  ctx.fillStyle = "#fff";
  ramp.forEach((r, i) => ctx.fillText(r.g, i * stride + 1 + cw / 2, 1 + baseline));

  gl.bindTexture(gl.TEXTURE_2D, atlasTex || (atlasTex = gl.createTexture()));
  gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, gl.RGBA, gl.UNSIGNED_BYTE, cv);
  for (const k of ["TEXTURE_MIN_FILTER", "TEXTURE_MAG_FILTER"])
    gl.texParameteri(gl.TEXTURE_2D, gl[k], gl.NEAREST);
  for (const k of ["TEXTURE_WRAP_S", "TEXTURE_WRAP_T"])
    gl.texParameteri(gl.TEXTURE_2D, gl[k], gl.CLAMP_TO_EDGE);

  return { w: cv.width, h: cv.height, stride, inkW: cw };
}

// --- sizing ----------------------------------------------------------------
// The canvas is sized FROM the character grid, never stretched to fit: a
// fractional cell-to-pixel ratio makes the glyphs shimmer. It can end up a few
// pixels short of the viewport, and that sliver shows the page ground, which
// is the same colour the scene fades to.

function resize() {
  // measure the SECTION, never the canvas: the canvas is absolutely
  // positioned and sized from this result, so reading its own box would be
  // circular and it would come back a few pixels tall
  const host = canvas.parentElement;
  const next = Math.max(1, Math.min(2, Math.round(window.devicePixelRatio || 1)));
  const c = Math.max(40, Math.min(MAX_COLS, Math.floor(host.clientWidth / CELL.w)));
  const r = Math.max(20, Math.min(MAX_ROWS, Math.floor(host.clientHeight / CELL.h)));
  if (c === cols && r === rows && next === dpr) return false;

  const rebuildAtlas = next !== dpr;
  cols = c; rows = r; dpr = next;
  canvas.width = cols * CELL.w * dpr;
  canvas.height = rows * CELL.h * dpr;
  canvas.style.width = cols * CELL.w + "px";
  canvas.style.height = rows * CELL.h + "px";
  if (rebuildAtlas && atlas) {
    const px = Math.round((CELL.w * dpr) / 0.6);   // Plex Mono advance is 0.6em
    atlas = buildAtlas(CELL.w * dpr, CELL.h * dpr, px);
  }
  return true;
}

// --- draw ------------------------------------------------------------------

function draw(now) {
  const t = reduced.matches ? SCENE.pose : (now - t0) / 1000;

  gl.bindFramebuffer(gl.FRAMEBUFFER, fbo);
  gl.viewport(0, 0, cols, rows);
  gl.useProgram(sceneProg);
  const u = sceneProg.uni;
  gl.uniform1f(u.uTime, t);
  gl.uniform2f(u.uGrid, cols, rows);
  gl.uniform1f(u.uAspect, (cols * CELL.w) / (rows * CELL.h));
  gl.uniform1f(u.uTanHalf, SCENE.tan);
  gl.uniform1f(u.uFar, SCENE.far);
  gl.uniform1f(u.uFog, SCENE.fog);
  gl.uniform1f(u.uBlack, SCENE.black);
  gl.uniform1f(u.uWhite, SCENE.white);
  gl.uniform3f(u.uKey, SCENE.key[0], SCENE.key[1], SCENE.key[2]);
  gl.uniform1f(u.uHead, SCENE.head);
  gl.uniform1f(u.uStepScale, SCENE.scale);
  gl.uniform1f(u.uParam, 0);
  gl.uniform1i(u.uSteps, SCENE.steps);
  gl.uniform1i(u.uShadow, SCENE.shadow);
  gl.drawArrays(gl.TRIANGLES, 0, 3);

  gl.bindFramebuffer(gl.FRAMEBUFFER, null);
  gl.viewport(0, 0, canvas.width, canvas.height);
  gl.useProgram(asciiProg);
  const a = asciiProg.uni;
  gl.activeTexture(gl.TEXTURE0); gl.bindTexture(gl.TEXTURE_2D, sceneTex);
  gl.activeTexture(gl.TEXTURE1); gl.bindTexture(gl.TEXTURE_2D, atlasTex);
  gl.uniform1i(a.uScene, 0);
  gl.uniform1i(a.uAtlas, 1);
  gl.uniform2f(a.uGrid, cols, rows);
  gl.uniform2f(a.uAtlasSize, atlas.w, atlas.h);
  gl.uniform1f(a.uAtlasStride, atlas.stride);
  gl.uniform1f(a.uAtlasInkW, atlas.inkW);
  gl.uniform1i(a.uGlyphs, GLYPHS);
  gl.drawArrays(gl.TRIANGLES, 0, 3);
}

// --- loop ------------------------------------------------------------------
// Self-cancelling: scrolled past, tab hidden, or reduced-motion honoured and
// the loop stops outright rather than spinning over a frame nobody sees.

function tick(now) {
  raf = 0;
  if (!running()) return;
  if (now - last >= FRAME_MS) { last = now; draw(now); }
  raf = requestAnimationFrame(tick);
}

function running() {
  return onScreen && !document.hidden && !reduced.matches;
}

function schedule() {
  if (!atlas) return;
  if (running()) { if (!raf) raf = requestAnimationFrame(tick); }
  else { if (raf) cancelAnimationFrame(raf); raf = 0; draw(performance.now()); }
}

// --- boot ------------------------------------------------------------------

(async function main() {
  if (!canvas) return;
  if (!gl) { document.body.classList.add("no-hero"); return; }
  if (!glInit()) { document.body.classList.add("no-hero"); return; }

  resize();
  const px = Math.round((CELL.w * dpr) / 0.6);
  await ensureFont(px);
  atlas = buildAtlas(CELL.w * dpr, CELL.h * dpr, px);
  if (!atlas) { document.body.classList.add("no-hero"); return; }

  t0 = performance.now();
  canvas.classList.add("ready");
  schedule();

  addEventListener("resize", () => { if (resize()) schedule(); });
  document.addEventListener("visibilitychange", schedule);
  reduced.addEventListener("change", schedule);
  canvas.addEventListener("webglcontextlost", (e) => e.preventDefault());

  new IntersectionObserver((ents) => {
    onScreen = ents[0].isIntersecting;
    schedule();
  }, { rootMargin: "120px" }).observe(canvas);
})();
