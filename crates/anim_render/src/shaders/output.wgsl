// Transfer conversion and export output (spec 15.3).
//
// DECISIONS.md #40 ruling 3: scene_output is Rgba8Unorm, NOT the Srgb variant, and the
// linear->sRGB encode happens HERE. The reason is testability — an Srgb format puts one
// transfer in fixed-function state where the golden cannot attribute it, and the moment a
// second transfer appears anywhere in the chain the two are indistinguishable in the
// output. With a linear-storage format the transfer is a named stage in the pass log, so
// 15.12's "exactly one output transfer" is a claim about a stage rather than about hardware.
//
// This is therefore the ONLY encode in the renderer. The decode happened once in
// anim_assets against the manifest's recorded claim (#41 ruling 2); nothing between that
// and here touches the transfer function.
//
// `pow` is a driver transcendental. That is acceptable because spec 7.1 pins GPU pixel
// identity to the tested device, backend, driver and build, and cross-device tolerance is
// WP11's ADR-0016 — not a number this package invents.

struct OutputVertexOut {
  @builtin(position) clip: vec4<f32>,
  @location(0) uv: vec2<f32>,
}

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

@vertex
fn vs_fullscreen(@builtin(vertex_index) vertex_index: u32) -> OutputVertexOut {
  let uv = vec2<f32>(f32((vertex_index << 1u) & 2u), f32(vertex_index & 2u));
  var out: OutputVertexOut;
  out.clip = vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
  out.uv = uv;
  return out;
}

// The sRGB OETF, inverse of the EOTF anim_assets applied at decode. The breakpoint and the
// 2.4 exponent are the IEC 61966-2-1 constants, not a tuned approximation.
fn linear_to_srgb(linear: vec3<f32>) -> vec3<f32> {
  let cutoff = vec3<f32>(0.0031308);
  let low = linear * 12.92;
  let high = 1.055 * pow(max(linear, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - 0.055;
  return select(high, low, linear <= cutoff);
}

// 15.7: "The final alpha mode converts premultiplied output to straight only when the
// selected codec requires it, with zero-alpha RGB explicitly defined." Two entry points
// rather than a uniform branch, so the choice is visible in the pipeline and therefore in
// the pass log. ZERO_ALPHA_RGB is alpha::ZERO_ALPHA_RGB = [0,0,0], the value that makes
// premul -> straight -> premul a fixed point.
fn to_straight(premultiplied: vec4<f32>) -> vec4<f32> {
  if (premultiplied.a <= 0.0) {
    return vec4<f32>(0.0, 0.0, 0.0, 0.0);
  }
  return vec4<f32>(premultiplied.rgb / premultiplied.a, premultiplied.a);
}

@fragment
fn fs_premultiplied(in: OutputVertexOut) -> @location(0) vec4<f32> {
  let texel = textureSample(source, source_sampler, in.uv);
  return vec4<f32>(linear_to_srgb(texel.rgb), texel.a);
}

@fragment
fn fs_straight(in: OutputVertexOut) -> @location(0) vec4<f32> {
  let texel = textureSample(source, source_sampler, in.uv);
  let straight = to_straight(texel);
  return vec4<f32>(linear_to_srgb(straight.rgb), straight.a);
}

// The editor UI composite (graph::composite_editor_ui). No transfer: the UI texture is
// already display-referred, because egui produces it for a surface rather than for a
// scene-referred target. Encoding it again would be the second transfer 15.12 forbids.
@fragment
fn ui_main(in: OutputVertexOut) -> @location(0) vec4<f32> {
  return textureSample(source, source_sampler, in.uv);
}