// Shared scene vertex and MRT logic (spec 15.3).
//
// The two structs below are the WGSL half of the contract declared in resources.rs. They
// are repeated verbatim in ordered_2d.wgsl and transparent.wgsl because wgpu has no shader
// include; `the_uniform_structs_agree_across_every_shader` in tests/golden.rs compares the
// three copies byte for byte, so drift is a test failure rather than a sheared golden.
//
// The texture is already LINEAR: anim_assets applied the transfer against the manifest's
// recorded claim at decode (DECISIONS.md #41 ruling 2), so there is no sRGB decode here.
// That is deliberate — #40 ruling 3's reasoning applies to every transfer in the chain, not
// only the output one: a transfer the shader cannot name is a transfer the golden cannot
// attribute. Premultiplication is the one colour operation left, and params.y decides it.

struct SceneUniform {
  view_proj: mat4x4<f32>,
  previous_view_proj: mat4x4<f32>,
  extent: vec2<f32>,
  history_valid: f32,
  _pad: f32,
}

struct ItemUniform {
  current_transform: mat4x4<f32>,
  previous_transform: mat4x4<f32>,
  tint: vec4<f32>,
  params: vec4<f32>,
  half_size: vec2<f32>,
  _pad2: vec2<f32>,
}

@group(0) @binding(0) var<uniform> scene: SceneUniform;
@group(1) @binding(0) var<uniform> item: ItemUniform;
@group(1) @binding(1) var albedo: texture_2d<f32>;
@group(1) @binding(2) var albedo_sampler: sampler;

struct VertexOut {
  @builtin(position) clip: vec4<f32>,
  @location(0) uv: vec2<f32>,
  @location(1) motion: vec2<f32>,
  @location(2) validity: f32,
  @location(3) view_normal: vec3<f32>,
}

// Two triangles, corners in [-1, 1], scaled by half_size in the vertex shader. Generated
// from vertex_index so there is no vertex buffer whose stride would be a third layout to
// keep in agreement.
const CORNERS = array<vec2<f32>, 6>(
  vec2<f32>(-1.0, -1.0),
  vec2<f32>( 1.0, -1.0),
  vec2<f32>(-1.0,  1.0),
  vec2<f32>(-1.0,  1.0),
  vec2<f32>( 1.0, -1.0),
  vec2<f32>( 1.0,  1.0),
);

// 15.7's own formula: uv = ndc_xy * vec2(0.5, -0.5) + 0.5.
fn ndc_to_uv(ndc: vec2<f32>) -> vec2<f32> {
  return ndc * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOut {
  let corner = CORNERS[vertex_index];
  let local = vec4<f32>(corner * item.half_size, 0.0, 1.0);

  let world = item.current_transform * local;
  let clip = scene.view_proj * world;

  let previous_world = item.previous_transform * local;
  let previous_clip = scene.previous_view_proj * previous_world;

  // 15.7: "If previous w <= 0 ... set validity to zero." With the orthographic projection
  // in camera.rs w is always 1, so this branch is the guard for a perspective rig WP15
  // authors later rather than a live condition today.
  var valid = scene.history_valid * item.params.w;
  if (previous_clip.w <= 0.0 || clip.w <= 0.0) {
    valid = 0.0;
  }

  var motion = vec2<f32>(0.0);
  if (valid > 0.5) {
    let current_ndc = clip.xy / clip.w;
    let previous_ndc = previous_clip.xy / previous_clip.w;
    motion = ndc_to_uv(current_ndc) - ndc_to_uv(previous_ndc);
  }

  var out: VertexOut;
  out.clip = clip;
  // corner (-1,-1) is the top-left of an authored +y-down quad, and texture (0,0) is its
  // top-left, so the mapping is the identity on the half-extent rather than a y flip.
  out.uv = corner * 0.5 + vec2<f32>(0.5);
  out.motion = motion;
  out.validity = valid;
  // A 2D quad faces the viewer. There is no authored normal in WP04's EvaluatedNode, so
  // this is the flat normal of the content class, with validity 1 in the fragment's w.
  out.view_normal = vec3<f32>(0.0, 0.0, 1.0);
  return out;
}

struct FragmentOut {
  @location(0) color: vec4<f32>,
  @location(1) normal: vec4<f32>,
  @location(2) velocity: vec4<f32>,
}

@fragment
fn fs_main(in: VertexOut) -> FragmentOut {
  let texel = textureSample(albedo, albedo_sampler, in.uv);

  // params.z is the alpha mode id from alpha::mode_id. 1 is AlphaTest, and 15.7 requires
  // the discard to happen "before attachment writes" — a discarded fragment writes no
  // depth either, so an alpha-tested sprite cannot leave a depth hole shaped like its quad.
  let mode = u32(item.params.z);
  if (mode == 1u && texel.a < item.params.x) {
    discard;
  }

  // params.y is alpha::premultiply_for, computed on the CPU so 15.7's "not multiplied
  // again" rule has one producer. The multiply is in linear light because the decode
  // already linearized.
  var rgb = texel.rgb * item.tint.rgb;
  if (item.params.y > 0.5) {
    rgb = rgb * texel.a;
  }
  var alpha = texel.a * item.tint.a;
  if (mode == 0u) {
    alpha = 1.0;
  }

  var out: FragmentOut;
  out.color = vec4<f32>(rgb, alpha);
  out.normal = vec4<f32>(in.view_normal, 1.0);
  // 15.6: velocity is "current UV minus previous UV, validity and optional reactive mask".
  // The reactive mask is WP14's; w carries validity alone here.
  out.velocity = vec4<f32>(in.motion, in.validity, 0.0);
  return out;
}