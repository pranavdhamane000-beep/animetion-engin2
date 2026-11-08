// DOF, motion blur, bloom and tone-map entry points (spec 15.3).
//
// DECISIONS.md #40 ruling 4: "implement the chain, the ping-pong, the versioned order, the
// disabled-pass rule and exactly one tone map. DOF, motion blur and bloom are
// declared-and-disabled. Their kernels are not this package's design decisions, and writing
// them now invents parameters WP14/WP17 will replace."
//
// So the three effect entry points below are the DISABLED-STAGE IDENTITY, not effects. They
// are unreachable: `PostChainConfig::validate` refuses to enable any of them with
// `RenderError::PostStageUnavailable` naming WP14/WP17, so no pipeline is ever created
// against them. They exist because 15.3 names this file as their home and a later package
// needs the entry point to already be declared. An identity is the only body that does not
// invent a parameter.
//
// The tone map IS implemented, and it is the only one: 15.6 says "Tone mapping runs once".

struct PostVertexOut {
  @builtin(position) clip: vec4<f32>,
  @location(0) uv: vec2<f32>,
}

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

// One full-screen triangle from vertex_index, so there is no vertex buffer. uv (0,0) is the
// top-left of the source texture and clip (-1,1) is the top-left of the target, which is the
// same orientation 15.7's ndc_to_uv produces.
@vertex
fn vs_fullscreen(@builtin(vertex_index) vertex_index: u32) -> PostVertexOut {
  let uv = vec2<f32>(f32((vertex_index << 1u) & 2u), f32(vertex_index & 2u));
  var out: PostVertexOut;
  out.clip = vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
  out.uv = uv;
  return out;
}

// --- Declared and disabled. WP14/WP17 replace these bodies. ---

@fragment
fn fs_depth_aware(in: PostVertexOut) -> @location(0) vec4<f32> {
  return textureSample(source, source_sampler, in.uv);
}

@fragment
fn fs_motion_blur(in: PostVertexOut) -> @location(0) vec4<f32> {
  return textureSample(source, source_sampler, in.uv);
}

@fragment
fn fs_bloom(in: PostVertexOut) -> @location(0) vec4<f32> {
  return textureSample(source, source_sampler, in.uv);
}

// --- The one tone map. ---

// Luminance weights are Rec.709, the primaries a linear scene-referred target is expressed
// in. A luminance-based Reinhard preserves hue where a per-channel one shifts it, which for
// an animation engine is the difference between a graded look and a colour cast.
const LUMA = vec3<f32>(0.2126, 0.7152, 0.0722);

// The version of this operator. A golden reproduces across a config change but not across
// an operator change, and this is the number that distinguishes them (ADR-0011).
const TONE_MAP_VERSION: u32 = 1u;

@fragment
fn fs_tone_map(in: PostVertexOut) -> @location(0) vec4<f32> {
  let hdr = textureSample(source, source_sampler, in.uv);
  let luma = dot(hdr.rgb, LUMA);
  // Reinhard: L / (1 + L). At L = 0 the ratio below is 0/0, so the mapped colour is zero
  // explicitly rather than left to the driver's division behaviour.
  var mapped = vec3<f32>(0.0);
  if (luma > 0.0) {
    let scale = (luma / (1.0 + luma)) / luma;
    mapped = hdr.rgb * scale;
  }
  // Alpha passes through: tone mapping is a colour operation, and changing coverage here
  // would make the output transfer's alpha conversion see a different image.
  return vec4<f32>(mapped, hdr.a);
}