// Ordered 2D material composition (spec 15.3).
//
// 15.6: "Ordered unlit 2D content follows project layer order; blended 2D items do not
// write auxiliary normals/velocity unless a future material contract explicitly supports
// it." There is no such contract, so this shader has ONE colour output and no normal or
// velocity attachment at all — the limitation is in the pipeline descriptor, not in a
// shader that writes zeros and pretends the target is populated.
//
// The struct block and vertex stage are byte-identical to scene.wgsl; the test
// `the_uniform_structs_agree_across_every_shader` compares them.

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
}

const CORNERS = array<vec2<f32>, 6>(
  vec2<f32>(-1.0, -1.0),
  vec2<f32>( 1.0, -1.0),
  vec2<f32>(-1.0,  1.0),
  vec2<f32>(-1.0,  1.0),
  vec2<f32>( 1.0, -1.0),
  vec2<f32>( 1.0,  1.0),
);

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOut {
  let corner = CORNERS[vertex_index];
  let local = vec4<f32>(corner * item.half_size, 0.0, 1.0);
  var out: VertexOut;
  out.clip = scene.view_proj * item.current_transform * local;
  out.uv = corner * 0.5 + vec2<f32>(0.5);
  return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
  let texel = textureSample(albedo, albedo_sampler, in.uv);
  var rgb = texel.rgb * item.tint.rgb;
  // The blend descriptor is One/OneMinusSrcAlpha (15.7's standard premultiplied-over), so
  // the shader's job is to hand it premultiplied RGB. params.y is alpha::premultiply_for.
  if (item.params.y > 0.5) {
    rgb = rgb * texel.a;
  }
  let alpha = texel.a * item.tint.a;
  return vec4<f32>(rgb, alpha);
}