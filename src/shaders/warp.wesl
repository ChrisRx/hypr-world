// A still of a workspace in the shadow realm (shadow.rs), slowly
// warping: rippling across and down, breathing in and out from the
// middle, its colors pulled a little apart and toward fel green, darker at
// its edges, and as deep in the murk as Bevy's exponential fog would have
// it. All of it sets in with `warp.x`,
// from the still as it was taken.

import bevy_pbr::render::forward_io::VertexOutput;

struct WarpParams {
    // The murk's linear color, and its density in w.
    murk: vec4<f32>,
    // The fel green, linear.
    fel: vec4<f32>,
    // The eye, and the time in seconds in w.
    eye: vec4<f32>,
    // How far the warp has set in, from 0 to 1, in x.
    warp: vec4<f32>,
}

@group(constants::MATERIAL_BIND_GROUP) @binding(0) var<uniform> params: WarpParams;
@group(constants::MATERIAL_BIND_GROUP) @binding(1) var still_texture: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(2) var still_sampler: sampler;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    var uv = vec2<f32>(0.0);
    @if(VERTEX_UVS_A) {
        uv = in.uv;
    }
    let t = params.eye.w;
    let k = params.warp.x;

    let ripple = vec2<f32>(
        sin(uv.y * 9.0 + t * 0.7) * 0.012 + sin(uv.y * 23.0 - t * 1.3) * 0.004,
        sin(uv.x * 7.0 - t * 0.5) * 0.010 + sin((uv.x + uv.y) * 17.0 + t * 0.9) * 0.004,
    );
    let middle = uv - vec2<f32>(0.5);
    let breath = 1.0 + 0.12 * sin(t * 0.4) * dot(middle, middle);
    let at = vec2<f32>(0.5) + middle * mix(1.0, breath, k) + ripple * k;

    let apart = vec2<f32>(0.004 * k, 0.0);
    var color = vec3<f32>(
        textureSample(still_texture, still_sampler, at + apart).r,
        textureSample(still_texture, still_sampler, at).g,
        textureSample(still_texture, still_sampler, at - apart).b,
    );
    let luma = dot(color, vec3<f32>(0.2126, 0.7152, 0.0722));
    color = mix(color, params.fel.rgb * luma, 0.45 * k);
    let edge = smoothstep(0.75, 0.3, length(middle));
    color = color * mix(1.0, 0.5 + 0.5 * edge, k);

    let seen = exp(-params.murk.w * distance(in.world_position.xyz, params.eye.xyz));
    return vec4<f32>(mix(params.murk.rgb, color, seen), 1.0);
}
