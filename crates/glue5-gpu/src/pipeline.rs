//! Render pipelines, by content: a [`Recipe`] is everything a pipeline is
//! made of (shaders, input layout, strides, states, target formats). Recipes
//! seen are kept on disk and, with the app's seed, compiled before the game
//! starts on later runs (see `prepare.rs`).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use rustc_hash::FxHashMap;
use serde_json::{Value, json};
use wgpu::{Features, TextureFormat};

use crate::format::{self, Caps};
use crate::shader::{self, Program, Stage};

/// One element of a vertex shader's input signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SigElement {
    pub semantic: u32,
    pub index: u32,
    pub register: u32,
}

/// One element of an input layout: semantic hash, semantic index, DXGI
/// format, slot, offset, class (1 = per instance), step rate.
pub type Element = [u32; 7];

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Recipe {
    pub vs: u64,
    pub ps: Option<u64>,
    /// The vertex shader's input signature (from the engine when it creates
    /// the shader): it places the layout's elements.
    pub sig: Arc<[SigElement]>,
    pub elements: Vec<Element>,
    /// Stride of each slot the layout uses, in [`slots`] order (0: unbound).
    pub strides: Vec<u32>,
    pub topology: u32,
    /// Strip index format: 0 none, 1 uint16, 2 uint32.
    pub strip: u8,
    pub blend: Option<Box<[u32]>>,
    pub depth: Option<Box<[u32]>>,
    /// Without its depth bias (see [`without_depth_bias`]).
    pub raster: Option<Box<[u32]>>,
    pub targets: Vec<Option<TextureFormat>>,
    pub samples: u32,
    pub depth_format: Option<TextureFormat>,
    /// 1: read-only depth, 2: read-only stencil.
    pub read_only: u8,
    /// The programs' variants for what is bound (`Program::for_bindings`):
    /// vertex unfiltered and depth, pixel unfiltered and depth (bits by SRV
    /// slot; unfiltered includes the depth bits). Recorded as on a GPU that
    /// cannot filter 32-bit floats, the same on every device (see
    /// [`Recipe::for_device`]).
    pub variant: [u128; 4],
    /// Uses the HUD coverage program rather than the original depth blit.
    pub hud_mask: bool,
}

/// The slots an input layout uses, each once, in element order, with the
/// class of its first element.
pub fn slots(elements: &[Element]) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for e in elements {
        if e[3] < 32 && !out.iter().any(|(s, _)| *s == e[3]) {
            out.push((e[3], e[5]));
        }
    }
    out
}

/// A D3D11 rasterizer state's words without its depth bias (DepthBias,
/// DepthBiasClamp, SlopeScaledDepthBias): draws set it on the render pass
/// (`RenderPass::set_depth_bias`, our wgpu patch), as Vulkan's dynamic
/// state. The engine's shadows use about two hundred biases (by cascade and
/// light): with them in the pipeline, each would be another pipeline.
pub fn without_depth_bias(words: &[u32]) -> Box<[u32]> {
    let mut words: Box<[u32]> = words.into();
    for w in words.iter_mut().skip(3).take(3) {
        *w = 0;
    }
    words
}

/// What a vertex buffer slot must hold for n elements: (n - 1) * stride + last.
#[derive(Clone, Copy, Debug)]
pub struct VertexNeed {
    pub slot: u32,
    pub instance: bool,
    pub stride: u32,
    pub last: u32,
}

pub struct Pipe {
    pub pipeline: wgpu::RenderPipeline,
    pub needs: Vec<VertexNeed>,
}

/// Set once the compile is done; `None`: the pipeline is not valid.
pub type Cell = OnceLock<Option<Pipe>>;

/// What compiling needs, shared with the compile threads.
pub struct Compiler {
    pub device: wgpu::Device,
    pub caps: Caps,
    pub max_inter_stage: u32,
    pub cache: Option<wgpu::PipelineCache>,
    null_ps: OnceLock<Option<wgpu::ShaderModule>>,
    /// See [`Compiler::clipping_ps`].
    clipping_ps: OnceLock<Option<wgpu::ShaderModule>>,
    layouts: Mutex<FxHashMap<(u32, u32), wgpu::PipelineLayout>>,
    pub compiled: AtomicU32,
    pub failed: AtomicU32,
    pub compile_us: AtomicU64,
}

impl Compiler {
    pub fn new(device: wgpu::Device, caps: Caps, cache: Option<wgpu::PipelineCache>) -> Compiler {
        let max_inter_stage = device.limits().max_inter_stage_shader_variables;
        Compiler {
            device,
            caps,
            max_inter_stage,
            cache,
            null_ps: OnceLock::new(),
            clipping_ps: OnceLock::new(),
            layouts: Mutex::default(),
            compiled: AtomicU32::new(0),
            failed: AtomicU32::new(0),
            compile_us: AtomicU64::new(0),
        }
    }

    fn layout(&self, vs: &Program, ps: Option<&Program>) -> wgpu::PipelineLayout {
        let key = (vs.id, ps.map_or(0, |p| p.id));
        self.layouts
            .lock()
            .unwrap()
            .entry(key)
            .or_insert_with(|| {
                self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: None,
                    bind_group_layouts: &[vs.layout.as_ref(), ps.and_then(|p| p.layout.as_ref())],
                    immediate_size: 0,
                })
            })
            .clone()
    }

    /// A pixel stage that writes nothing: D3D draws without a pixel shader
    /// still have their render targets bound.
    fn null_ps(&self) -> Option<&wgpu::ShaderModule> {
        self.null_ps
            .get_or_init(|| shader::create_module(&self.device, "null ps", "@fragment fn main() {}\n").ok())
            .as_ref()
    }

    /// A pixel stage that only clips, for draws without a pixel shader on
    /// GPUs without clip distances (`at`: where the distances arrive).
    fn clipping_ps(&self, at: u32) -> Option<wgpu::ShaderModule> {
        let make = || shader::create_module(&self.device, "clipping ps", &shader::with_clipping(None, at)?).ok();
        if at == shader::CLIP_LOCATION { self.clipping_ps.get_or_init(make).clone() } else { make() }
    }

    /// Builds the pipeline (any thread). Failures are logged and give `None`.
    pub fn compile(&self, vs: &Program, ps: Option<&Program>, r: &Recipe) -> Option<Pipe> {
        let t0 = Instant::now();
        let label = format!("{} + {}", vs.label, ps.map_or("-", |p| &p.label));
        let result = self.build(vs, ps, r, &label);
        self.compile_us.fetch_add(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
        match result {
            Ok(pipe) => {
                self.compiled.fetch_add(1, Ordering::Relaxed);
                Some(pipe)
            }
            Err(e) => {
                self.failed.fetch_add(1, Ordering::Relaxed);
                crate::log_once(&format!("pipe{label}{:?}", r.targets), || format!("pipeline {label} failed: {e:#}"));
                None
            }
        }
    }

    fn build(&self, vs: &Program, ps: Option<&Program>, r: &Recipe, label: &str) -> anyhow::Result<Pipe> {
        let features = self.caps.features;
        let remap = shader::remap_locations(vs, ps, self.max_inter_stage);
        let interpolation = ps.map_or(&[][..], |p| &p.interpolation);
        let hud_mask = ps.is_some_and(|p| p.hud_mask);

        // Vertex stage: outputs take the pixel shader's interpolation, and move
        // below the device's inter-stage limit if needed.
        let vs_module = if !hud_mask && interpolation.is_empty() && remap.is_none() {
            vs.module(&self.device).ok_or_else(|| anyhow::anyhow!("vertex module failed"))?
        } else {
            let key = format!("{interpolation:?}|{remap:?}|{hud_mask}");
            let hud_text = if hud_mask {
                Some(
                    shader::with_hud_coordinates(&vs.text.get())
                        .ok_or_else(|| anyhow::anyhow!("unexpected HUD mask vertices"))?,
                )
            } else {
                None
            };
            vs.variant(&self.device, &key, |text| {
                let text = shader::with_interpolation(hud_text.as_deref().unwrap_or(text), interpolation);
                match &remap {
                    Some(map) => shader::with_locations(&text, Stage::Vertex, map),
                    None => text,
                }
            })
            .ok_or_else(|| anyhow::anyhow!("vertex variant failed"))?
        };

        // Vertex buffers: layout elements matched to the shader's inputs by semantic.
        let slot_list = slots(&r.elements);
        let mut attributes: Vec<(u32, wgpu::VertexAttribute)> = Vec::new();
        for e in &r.elements {
            let Some(s) = r.sig.iter().find(|s| s.semantic == e[0] && s.index == e[1]) else {
                continue;
            };
            let Some(format) = format::vertex(e[2]) else {
                crate::log_once(&format!("vf{}", e[2]), || format!("unsupported vertex format DXGI {}", e[2]));
                continue;
            };
            attributes.push((e[3], wgpu::VertexAttribute { format, offset: e[4] as u64, shader_location: s.register }));
        }
        let mut buffers: Vec<Option<wgpu::VertexBufferLayout>> = Vec::new();
        let mut needs = Vec::new();
        let mut attrs_by_slot: Vec<Vec<wgpu::VertexAttribute>> = Vec::new();
        let mut used_slots = Vec::new();
        for (i, &(slot, class)) in slot_list.iter().enumerate() {
            let attrs: Vec<_> = attributes.iter().filter(|(s, _)| *s == slot).map(|(_, a)| *a).collect();
            if attrs.is_empty() {
                continue;
            }
            let stride = r.strides.get(i).copied().unwrap_or(0);
            let last = attrs.iter().map(|a| a.offset as u32 + a.format.size() as u32).max().unwrap_or(0);
            needs.push(VertexNeed { slot, instance: class == 1, stride, last });
            used_slots.push((slot, class, stride));
            attrs_by_slot.push(attrs);
        }
        for ((slot, class, stride), attrs) in used_slots.iter().zip(&attrs_by_slot) {
            let slot = *slot as usize;
            if buffers.len() <= slot {
                buffers.resize(slot + 1, None);
            }
            buffers[slot] = Some(wgpu::VertexBufferLayout {
                array_stride: *stride as u64,
                step_mode: if *class == 1 { wgpu::VertexStepMode::Instance } else { wgpu::VertexStepMode::Vertex },
                attributes: attrs,
            });
        }

        let (topology, _) = format::topology(r.topology);
        let mut primitive = wgpu::PrimitiveState {
            topology,
            strip_index_format: match r.strip {
                1 => Some(wgpu::IndexFormat::Uint16),
                2 => Some(wgpu::IndexFormat::Uint32),
                _ => None,
            },
            front_face: wgpu::FrontFace::Cw,
            cull_mode: Some(wgpu::Face::Back),
            ..Default::default()
        };
        // D3D11_RASTERIZER_DESC: FillMode, CullMode, FrontCCW, DepthBias,
        // DepthBiasClamp, SlopeScaledDepthBias, DepthClipEnable, Scissor, MS, AA lines
        if let Some(w) = &r.raster {
            primitive.cull_mode = match w[1] {
                2 => Some(wgpu::Face::Front),
                3 => Some(wgpu::Face::Back),
                _ => None,
            };
            primitive.front_face = if w[2] != 0 { wgpu::FrontFace::Ccw } else { wgpu::FrontFace::Cw };
            primitive.unclipped_depth = w[6] == 0 && features.contains(Features::DEPTH_CLIP_CONTROL);
        }

        let depth_stencil = r.depth_format.map(|format| {
            let mut d = wgpu::DepthStencilState {
                format,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: Default::default(),
                bias: Default::default(),
            };
            // D3D11_DEPTH_STENCIL_DESC: DepthEnable, DepthWriteMask, DepthFunc,
            // StencilEnable, masks (read | write << 8), front fail/depth fail/pass/func, back ...
            if let Some(w) = &r.depth {
                if w[0] != 0 {
                    d.depth_write_enabled = Some(w[1] == 1);
                    d.depth_compare = Some(format::compare(w[2]).unwrap_or(wgpu::CompareFunction::Less));
                }
                if format.has_stencil_aspect() && w[3] != 0 {
                    let face = |i: usize| wgpu::StencilFaceState {
                        fail_op: format::stencil_op(w[i]),
                        depth_fail_op: format::stencil_op(w[i + 1]),
                        pass_op: format::stencil_op(w[i + 2]),
                        compare: format::compare(w[i + 3]).unwrap_or(wgpu::CompareFunction::Always),
                    };
                    d.stencil = wgpu::StencilState {
                        front: face(5),
                        back: face(9),
                        read_mask: w[4] & 0xff,
                        write_mask: (w[4] >> 8) & 0xff,
                    };
                }
            }
            if r.read_only & 1 != 0 {
                d.depth_write_enabled = Some(false);
            }
            if r.read_only & 2 != 0 {
                d.stencil.write_mask = 0;
            }
            d
        });

        // D3D11_BLEND_DESC: AlphaToCoverage, IndependentBlend, 8 x {enable,
        // src, dst, op, srcA, dstA, opA, mask}
        let blend_words = r.blend.as_deref();
        let alpha_to_coverage = blend_words.is_some_and(|w| w[0] != 0);
        let mut targets: Vec<Option<wgpu::ColorTargetState>> = Vec::new();
        for (i, t) in r.targets.iter().enumerate() {
            let Some(format) = *t else {
                targets.push(None);
                continue;
            };
            let writes = ps.is_some_and(|p| p.outputs & (1 << i) != 0);
            let mut target = wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::empty() };
            if writes {
                target.write_mask = wgpu::ColorWrites::ALL;
                if let Some(w) = blend_words {
                    let rt = if w[1] != 0 { i } else { 0 };
                    let q = 2 + rt * 8;
                    target.write_mask = wgpu::ColorWrites::from_bits_truncate(w[q + 7] & 0xf);
                    if w[q] != 0 && format_blendable(format) {
                        let factor = |v: u32, default| {
                            dual_source_fallback(format::blend_factor(v).unwrap_or(default), features)
                        };
                        target.blend = Some(wgpu::BlendState {
                            color: wgpu::BlendComponent {
                                src_factor: factor(w[q + 1], wgpu::BlendFactor::One),
                                dst_factor: factor(w[q + 2], wgpu::BlendFactor::Zero),
                                operation: format::blend_op(w[q + 3]),
                            },
                            alpha: wgpu::BlendComponent {
                                src_factor: factor(w[q + 4], wgpu::BlendFactor::One),
                                dst_factor: factor(w[q + 5], wgpu::BlendFactor::Zero),
                                operation: format::blend_op(w[q + 6]),
                            },
                        });
                    }
                }
            }
            targets.push(Some(target));
        }

        // Without the GPU's clip distances the pixel stage clips, from the
        // copy the vertex stage writes (see shader::without_clip_distances).
        let clip = vs.clip_distances && !features.contains(wgpu::Features::CLIP_DISTANCES);
        let ps_module = match ps {
            Some(p) => {
                let alpha_test = alpha_to_coverage && r.samples <= 1 && p.outputs & 1 != 0;
                let reads = |l: &u32| p.locations.contains(l) || (clip && *l == shader::CLIP_LOCATION);
                let ps_remap = remap.as_ref().filter(|m| m.iter().any(|(from, _)| reads(from)));
                let no_depth = p.frag_depth && r.depth_format.is_none();
                if !alpha_test && !clip && ps_remap.is_none() && !no_depth {
                    Some(p.module(&self.device).ok_or_else(|| anyhow::anyhow!("pixel module failed"))?)
                } else {
                    let key = format!("{alpha_test}|{clip}|{ps_remap:?}|{no_depth}");
                    let module = p.variant(&self.device, &key, |text| {
                        let mut text = text.to_owned();
                        if alpha_test && let Some(t) = shader::with_alpha_test(&text) {
                            text = t;
                        }
                        if no_depth && let Some(t) = shader::without_frag_depth(&text) {
                            text = t;
                        }
                        if clip && let Some(t) = shader::with_clipping(Some(&text), shader::CLIP_LOCATION) {
                            text = t;
                        }
                        if let Some(map) = ps_remap {
                            text = shader::with_locations(&text, Stage::Pixel, map);
                        }
                        text
                    });
                    Some(module.ok_or_else(|| anyhow::anyhow!("pixel variant failed"))?)
                }
            }
            None if clip => {
                let moved = remap.iter().flatten().find(|(from, _)| *from == shader::CLIP_LOCATION);
                let at = moved.map_or(shader::CLIP_LOCATION, |m| m.1);
                Some(self.clipping_ps(at).ok_or_else(|| anyhow::anyhow!("clipping module failed"))?)
            }
            None if targets.iter().any(Option::is_some) => self.null_ps().cloned(),
            None => None,
        };

        let layout = self.layout(vs, ps);
        let options = wgpu::PipelineCompilationOptions { constants: &[], zero_initialize_workgroup_memory: false };
        let desc = wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &vs_module,
                entry_point: Some("main"),
                compilation_options: options.clone(),
                buffers: &buffers,
            },
            primitive,
            depth_stencil,
            multisample: wgpu::MultisampleState {
                count: r.samples.max(1),
                mask: !0,
                alpha_to_coverage_enabled: alpha_to_coverage && r.samples > 1,
            },
            fragment: ps_module.as_ref().map(|module| wgpu::FragmentState {
                module,
                entry_point: Some("main"),
                compilation_options: options.clone(),
                targets: &targets,
            }),
            multiview_mask: None,
            cache: self.cache.as_ref(),
        };
        let scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let pipeline = self.device.create_render_pipeline(&desc);
        if let Some(e) = pollster::block_on(scope.pop()) {
            anyhow::bail!("{e}");
        }
        Ok(Pipe { pipeline, needs })
    }
}

/// Blending on formats that cannot blend (32-bit float without the feature,
/// integer formats) is dropped rather than failing the pipeline.
fn format_blendable(f: TextureFormat) -> bool {
    !matches!(
        f,
        TextureFormat::R32Float
            | TextureFormat::Rg32Float
            | TextureFormat::Rgba32Float
            | TextureFormat::R32Uint
            | TextureFormat::R32Sint
    ) && f.sample_type(None, None).is_some_and(|t| matches!(t, wgpu::TextureSampleType::Float { .. }))
}

/// The translated shaders have no second blend source: dual-source factors
/// read the first one.
fn dual_source_fallback(f: wgpu::BlendFactor, _features: Features) -> wgpu::BlendFactor {
    use wgpu::BlendFactor as B;
    match f {
        B::Src1 => B::Src,
        B::OneMinusSrc1 => B::OneMinusSrc,
        B::Src1Alpha => B::SrcAlpha,
        B::OneMinusSrc1Alpha => B::OneMinusSrcAlpha,
        other => other,
    }
}

// ---- recipes on disk (the browser build's JSON format, with the signature) ----

impl Recipe {
    /// The pipeline this device makes of a recipe recorded on any device:
    /// recipes say what the game drew, as on a GPU with every format and
    /// unable to filter 32-bit floats. A GPU that filters them needs no
    /// unfilterable bindings; one without 32-bit float depth with stencil,
    /// or without 16-bit normalized formats, has their stand-ins (see
    /// `format::texture`). Recipes recorded here already have them.
    pub fn for_device(&self, caps: &Caps) -> Recipe {
        let mut r = self.clone();
        if caps.float32_filter {
            r.variant[0] = r.variant[1];
            r.variant[2] = r.variant[3];
        }
        let format = |f: TextureFormat| match f {
            TextureFormat::Depth32FloatStencil8 if !caps.features.contains(Features::DEPTH32FLOAT_STENCIL8) => {
                TextureFormat::Depth24PlusStencil8
            }
            TextureFormat::Rgba16Unorm if !caps.norm16 => TextureFormat::Rgba16Float,
            TextureFormat::Rg16Unorm if !caps.norm16 => TextureFormat::Rg16Float,
            f => f,
        };
        r.targets.iter_mut().flatten().for_each(|f| *f = format(*f));
        r.depth_format = r.depth_format.map(format);
        r
    }

    pub fn to_json(&self) -> Value {
        let words = |w: &Option<Box<[u32]>>| w.as_ref().map(|w| json!(w.to_vec()));
        let strip = ["", "uint16", "uint32"][self.strip as usize];
        let mut v = json!({
            "vs": format!("{:016x}", self.vs),
            "ps": self.ps.map(|h| format!("{h:016x}")).unwrap_or_default(),
            "sig": self.sig.iter().map(|s| [s.semantic, s.index, s.register]).collect::<Vec<_>>(),
            "el": self.elements,
            "st": self.strides,
            "tp": self.topology,
            "sf": strip,
            "b": words(&self.blend),
            "d": words(&self.depth),
            "r": words(&self.raster),
            "rt": self.targets.iter().map(|t| t.map(format::name)).collect::<Vec<_>>(),
            "n": self.samples,
            "ds": self.depth_format.map(format::name).unwrap_or(""),
            "ro": self.read_only,
            "hud_mask": self.hud_mask,
        });
        if self.variant != [0; 4] {
            v["variant"] = json!(self.variant.map(|m| format!("{m:x}")));
        }
        v
    }

    pub fn from_json(v: &Value) -> Option<Recipe> {
        let hex = |k: &str| u64::from_str_radix(v.get(k)?.as_str()?, 16).ok();
        let words = |k: &str| -> Option<Option<Box<[u32]>>> {
            match v.get(k)? {
                Value::Null => Some(None),
                Value::Array(a) => {
                    Some(Some(a.iter().map(|x| x.as_u64().map(|x| x as u32)).collect::<Option<Vec<_>>>()?.into()))
                }
                _ => None,
            }
        };
        let u = |k: &str| v.get(k).and_then(Value::as_u64).map(|x| x as u32);
        let elements = v
            .get("el")?
            .as_array()?
            .iter()
            .map(|e| {
                let a = e.as_array()?;
                let mut out = [0u32; 7];
                for (i, x) in a.iter().take(7).enumerate() {
                    out[i] = x.as_u64()? as u32;
                }
                Some(out)
            })
            .collect::<Option<Vec<_>>>()?;
        let sig = v
            .get("sig")?
            .as_array()?
            .iter()
            .map(|s| {
                let w = |i: usize| s.get(i).and_then(Value::as_u64).map(|x| x as u32);
                Some(SigElement { semantic: w(0)?, index: w(1)?, register: w(2)? })
            })
            .collect::<Option<_>>()?;
        Some(Recipe {
            vs: hex("vs")?,
            ps: hex("ps"),
            sig,
            elements,
            strides: v.get("st")?.as_array()?.iter().map(|x| x.as_u64().map(|x| x as u32)).collect::<Option<_>>()?,
            topology: u("tp")?,
            strip: match v.get("sf").and_then(Value::as_str) {
                Some("uint16") => 1,
                Some("uint32") => 2,
                _ => 0,
            },
            blend: words("b")?,
            depth: words("d")?,
            raster: words("r")?.map(|w| without_depth_bias(&w)),
            targets: v.get("rt")?.as_array()?.iter().map(|t| t.as_str().and_then(format::by_name)).collect(),
            samples: u("n").unwrap_or(1),
            depth_format: v.get("ds").and_then(Value::as_str).and_then(format::by_name),
            read_only: u("ro").unwrap_or(0) as u8,
            hud_mask: v.get("hud_mask").and_then(Value::as_bool).unwrap_or(false),
            variant: match v.get("variant") {
                None => [0; 4],
                Some(a) => {
                    let mask = |i: usize| u128::from_str_radix(a.get(i)?.as_str()?, 16).ok();
                    [mask(0)?, mask(1)?, mask(2)?, mask(3)?]
                }
            },
        })
    }
}

/// The recipes seen while playing this game build, gathered from several
/// devices and automated runs (`mise run shaders:seed`, `shaders:collect`).
const SEED: &str = include_str!("../seed/pipelines.jsonl");

/// The recipes known before the game starts: the seed and this device's
/// (`<cache>/pipelines.jsonl`), each once. Lines in another format (without
/// a signature) are skipped.
pub fn known_recipes(cache_dir: Option<&std::path::Path>) -> Vec<Arc<Recipe>> {
    let recorded = cache_dir.and_then(|d| std::fs::read_to_string(d.join("pipelines.jsonl")).ok()).unwrap_or_default();
    let mut seen = rustc_hash::FxHashSet::default();
    SEED.lines()
        .chain(recorded.lines())
        .filter_map(|l| Recipe::from_json(&serde_json::from_str(l).ok()?))
        .filter(|r| seen.insert(r.clone()))
        .map(Arc::new)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(features: Features, norm16: bool, float32_filter: bool) -> Caps {
        Caps { features, norm16, indirect: true, float32_filter, depth_bias_clamp: true }
    }

    fn recipe() -> Recipe {
        let line = SEED.lines().next().unwrap();
        let mut r = Recipe::from_json(&serde_json::from_str(line).unwrap()).unwrap();
        r.targets = vec![Some(TextureFormat::Rgba16Unorm), None, Some(TextureFormat::Rg16Unorm)];
        r.depth_format = Some(TextureFormat::Depth32FloatStencil8);
        // A 32-bit float texture in slot 0, a depth read in slot 1.
        r.variant = [0b11, 0b10, 0b01, 0];
        r
    }

    #[test]
    fn seed_parses() {
        let lines = SEED.lines().filter(|l| l.contains("\"sig\":")).count();
        assert_eq!(known_recipes(None).len(), lines);
    }

    #[test]
    fn a_gpu_with_everything_filters_float32() {
        let r = recipe().for_device(&caps(Features::DEPTH32FLOAT_STENCIL8, true, true));
        assert_eq!(r.variant, [0b10, 0b10, 0, 0]);
        assert_eq!(r.targets, recipe().targets);
        assert_eq!(r.depth_format, recipe().depth_format);
    }

    #[test]
    fn a_gpu_without_formats_has_their_stand_ins() {
        let r = recipe().for_device(&caps(Features::empty(), false, false));
        assert_eq!(r.variant, recipe().variant);
        assert_eq!(r.targets, [Some(TextureFormat::Rgba16Float), None, Some(TextureFormat::Rg16Float)]);
        assert_eq!(r.depth_format, Some(TextureFormat::Depth24PlusStencil8));
        assert_eq!(r.for_device(&caps(Features::empty(), false, false)), r);
    }
}
