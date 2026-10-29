// A window's surface in YUV (NV12, P010): its luma and chroma planes, each
// sampled through an image of its own, turned into RGB as Hyprland shows
// them. Hyprland imports a YUV dmabuf through EGL with no colour hints, so
// Mesa's defaults apply: BT.601 coefficients at limited range (luma from 16
// to 235, chroma from 16 to 240, out of 255). P010's samples sit in the top
// of 16 bits, so read as unorm they come out at the same scale.

import bevy_pbr::render::forward_io::VertexOutput;

struct YuvParams {
    // The quad's uv scaled by xy, then moved by zw, into the part of the
    // buffer the surface shows.
    crop: vec4<f32>,
}

@group(constants::MATERIAL_BIND_GROUP) @binding(0) var<uniform> yuv: YuvParams;
@group(constants::MATERIAL_BIND_GROUP) @binding(1) var luma_texture: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(2) var luma_sampler: sampler;
@group(constants::MATERIAL_BIND_GROUP) @binding(3) var chroma_texture: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(4) var chroma_sampler: sampler;

// Hyprland's RGB is sRGB-encoded, and this target takes linear colours, as
// an sRGB texture sampled gives them.
fn to_linear(c: vec3<f32>) -> vec3<f32> {
    let low = c / 12.92;
    let high = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(high, low, c <= vec3<f32>(0.04045));
}

// Limited-range BT.601 Y'CbCr, each 0 to 1 as sampled, to R'G'B'.
fn bt601(y: f32, cbcr: vec2<f32>) -> vec3<f32> {
    let l = (y - 16.0 / 255.0) * (255.0 / 219.0);
    let c = (cbcr - vec2<f32>(128.0 / 255.0)) * (255.0 / 224.0);
    return vec3<f32>(l + 1.402 * c.y, l - 0.344136 * c.x - 0.714136 * c.y, l + 1.772 * c.x);
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    var uv = vec2<f32>(0.0);
    @if(VERTEX_UVS_A) {
        uv = in.uv * yuv.crop.xy + yuv.crop.zw;
    }
    let y = textureSample(luma_texture, luma_sampler, uv).r;
    let cbcr = textureSample(chroma_texture, chroma_sampler, uv).rg;
    let rgb = clamp(bt601(y, cbcr), vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(to_linear(rgb), 1.0);
}
