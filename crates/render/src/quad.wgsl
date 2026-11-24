struct Uniforms {
    screen: vec2<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

struct Instance {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
};

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: Instance) -> VOut {
    var corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0),
    );
    let p = inst.pos + corners[vi] * inst.size;
    var out: VOut;
    out.pos = vec4(p.x / u.screen.x * 2.0 - 1.0, 1.0 - p.y / u.screen.y * 2.0, 0.0, 1.0);
    out.color = inst.color;
    return out;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    return in.color;
}
