//! Vulkan renderer (ash). Knows nothing about the game: it draws batches of
//! analytic shapes and pieces of one texture atlas, and owns presentation.
//!
//! A frame is deliberately split into separate calls so the caller controls
//! where time is spent and where the cursor is latched:
//!
//! ```text
//! acquire()        may block (FIFO); everything that waits happens here
//! batch().push()   write instances straight into mapped GPU memory
//! record()         build the command buffer
//! wait_previous()  never more than one frame queued on the GPU
//! latch_cursor()   newest cursor position into a mapped uniform buffer
//! submit_present() nothing between the latch and the GPU but two calls
//! ```

use ash::vk;
use std::ffi::c_void;

/// Handles needed to create a surface; the platform layer provides them.
#[derive(Clone, Copy, Debug)]
pub enum SurfaceHandle {
    Wayland { display: *mut c_void, surface: *mut c_void },
    Xcb { connection: *mut c_void, window: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentMode {
    /// Flip as soon as the frame is done. Lowest latency, may tear.
    Immediate,
    /// Newest finished frame wins at the next refresh; no tearing, no queue.
    Mailbox,
    FifoRelaxed,
    /// Classic vsync queue. Always available.
    Fifo,
}

impl PresentMode {
    /// Lowest latency first.
    pub const BY_LATENCY: [PresentMode; 4] =
        [PresentMode::Immediate, PresentMode::Mailbox, PresentMode::FifoRelaxed, PresentMode::Fifo];

    fn vk(self) -> vk::PresentModeKHR {
        match self {
            PresentMode::Immediate => vk::PresentModeKHR::IMMEDIATE,
            PresentMode::Mailbox => vk::PresentModeKHR::MAILBOX,
            PresentMode::FifoRelaxed => vk::PresentModeKHR::FIFO_RELAXED,
            PresentMode::Fifo => vk::PresentModeKHR::FIFO,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            PresentMode::Immediate => "IMMEDIATE",
            PresentMode::Mailbox => "MAILBOX",
            PresentMode::FifoRelaxed => "FIFO-RELAXED",
            PresentMode::Fifo => "FIFO",
        }
    }

    /// Whether the driver paces us (present/acquire block until a refresh).
    pub fn is_vsynced(self) -> bool {
        matches!(self, PresentMode::Fifo | PresentMode::FifoRelaxed)
    }
}

#[derive(Debug)]
pub struct GfxError(pub String);

impl std::fmt::Display for GfxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for GfxError {}
impl From<vk::Result> for GfxError {
    fn from(r: vk::Result) -> Self {
        GfxError(format!("Vulkan: {r:?}"))
    }
}
fn err<T>(msg: impl Into<String>) -> Result<T, GfxError> {
    Err(GfxError(msg.into()))
}

pub const KIND_RECT: u32 = 0;
pub const KIND_DISC: u32 = 1;
pub const KIND_RING: u32 = 2;
pub const KIND_GLYPH: u32 = 3;
pub const KIND_CAPSULE: u32 = 4;
pub const KIND_SPRITE: u32 = 5;
pub const KIND_RRECT: u32 = 6;
pub const KIND_BODY: u32 = 7;
pub const KIND_SECTOR: u32 = 8;
/// The textures a sprite can come from.
pub const PAGE_ATLAS: u32 = 0;
/// A second atlas, for the images of the menus.
pub const PAGE_UI: u32 = 1;
/// One large image: the background of the selected beatmap.
pub const PAGE_BACKGROUND: u32 = 2;
/// Size of the background texture; images are scaled down to fit into it.
pub const BACKGROUND_SIZE: (u32, u32) = (2560, 1440);
/// The atlas texture is square, this many pixels on a side.
pub const ATLAS_SIZE: u32 = 4096;
/// OR into `kind`: `center` is relative to the late-latched cursor.
pub const FLAG_AT_CURSOR: u32 = 256;
/// Translucent trail stamps blend with one another without writing depth.
pub const FLAG_TRAIL: u32 = 512;

/// One drawn shape. Layout is the vertex format; keep in sync with the
/// pipeline's attribute descriptions and shader.wgsl.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Inst {
    pub center: [f32; 2],
    pub half: [f32; 2],
    pub color: [u8; 4],
    pub kind: u32,
    pub p0: f32,
    pub p1: u32,
    /// Atlas rectangle (u0, v0, u1, v1), each 0..=65535 across the atlas.
    pub uv: [u16; 4],
}

/// Which slider body the following `Batch::body` segments belong to.
#[derive(Clone, Copy, Debug)]
pub struct BodyId(u32);

/// Write cursor into a frame's mapped instance buffer: the per-frame arena.
pub struct Batch {
    ptr: *mut Inst,
    len: usize,
    cap: usize,
    clips: Vec<(std::ops::Range<usize>, [f32; 4])>,
    clipping: bool,
}

impl Batch {
    /// A batch that writes into memory of the caller's instead of a frame:
    /// for exercising drawing code without a GPU.
    ///
    /// # Safety
    /// `ptr` must be valid for writing `cap` instances for as long as the
    /// batch is used.
    pub unsafe fn over(ptr: *mut Inst, cap: usize) -> Batch {
        Batch { ptr, len: 0, cap, clips: Vec::new(), clipping: false }
    }

    /// Scissor the instances emitted by `draw` to pixel edges [left, top,
    /// right, bottom]. Entries remain in the original arena and draw order.
    /// Scopes are sequential, not nested.
    pub fn clipped(&mut self, edges: [f32; 4], draw: impl FnOnce(&mut Self)) {
        assert!(!self.clipping, "nested batch clipping");
        self.clipping = true;
        let first = self.len;
        draw(self);
        self.clipping = false;
        if first != self.len {
            self.clips.push((first..self.len, edges));
        }
    }

    #[inline]
    pub fn push(&mut self, inst: Inst) {
        if self.len < self.cap {
            // SAFETY: ptr..ptr+cap is the mapped buffer of the current slot,
            // which the GPU is not reading (see `Gfx::wait_previous`).
            unsafe { self.ptr.add(self.len).write(inst) };
            self.len += 1;
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: [u8; 4]) {
        self.push(Inst {
            center: [x + w * 0.5, y + h * 0.5],
            half: [w * 0.5, h * 0.5],
            color,
            kind: KIND_RECT,
            p0: 0.0,
            p1: 0,
            uv: [0; 4],
        });
    }

    pub fn disc(&mut self, cx: f32, cy: f32, r: f32, color: [u8; 4], flags: u32) {
        self.push(Inst { center: [cx, cy], half: [r, r], color, kind: KIND_DISC | flags, p0: 0.0, p1: 0, uv: [0; 4] });
    }

    /// A disc sector filled clockwise from twelve o'clock.
    pub fn sector(&mut self, cx: f32, cy: f32, r: f32, progress: f32, color: [u8; 4]) {
        self.push(Inst { center: [cx, cy], half: [r, r], color, kind: KIND_SECTOR, p0: progress.clamp(0.0, 1.0), p1: 0, uv: [0; 4] });
    }

    /// A thick line from `a` to `b` with round ends: one segment of a
    /// slider body.
    pub fn capsule(&mut self, a: (f32, f32), b: (f32, f32), r: f32, color: [u8; 4]) {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        self.push(Inst {
            center: [(a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5],
            half: [dx.hypot(dy) * 0.5 + r, r],
            color,
            kind: KIND_CAPSULE,
            p0: dy.atan2(dx),
            p1: 0,
            uv: [0; 4],
        });
    }

    /// Start of a slider body: the `body` capsules that follow, up to the
    /// next call, are shaded as one shape (where they overlap, the one
    /// nearer to its own centre line wins).
    pub fn begin_body(&mut self) -> BodyId {
        BodyId(self.len as u32)
    }

    /// One segment of a slider body, shaded the way osu!stable does it: a
    /// border of `border`, then a gradient made from `track` (darker at the
    /// edge, lighter in the middle). `r` is the hit circle radius; the
    /// visible edge is a little inside it. `fade` (0..=255) is the body's
    /// opacity. A body must be given twice, all segments with `depth_only`
    /// set first and then all without: the first round settles which segment
    /// owns each pixel, the second blends only that one, so a translucent
    /// body is not darker where its segments overlap.
    #[allow(clippy::too_many_arguments)]
    pub fn body(
        &mut self,
        id: BodyId,
        a: (f32, f32),
        b: (f32, f32),
        r: f32,
        border: [u8; 3],
        track: [u8; 3],
        fade: u8,
        depth_only: bool,
    ) {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let w = |c: u8| c as u16 * 257;
        self.push(Inst {
            center: [(a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5],
            half: [dx.hypot(dy) * 0.5 + r, r],
            color: [track[0], track[1], track[2], fade],
            kind: KIND_BODY,
            p0: dy.atan2(dx),
            p1: id.0,
            uv: [w(border[0]), w(border[1]), w(border[2]), depth_only as u16],
        });
    }

    /// A rectangle of the atlas centred on (cx, cy), tinted by `color` and
    /// turned by `angle` radians.
    #[allow(clippy::too_many_arguments)]
    pub fn sprite(&mut self, cx: f32, cy: f32, half: [f32; 2], uv: [u16; 4], color: [u8; 4], angle: f32, flags: u32) {
        self.push(Inst { center: [cx, cy], half, color, kind: KIND_SPRITE | flags, p0: angle, p1: PAGE_ATLAS, uv });
    }

    /// A sprite from one of the other textures (`PAGE_*`).
    #[allow(clippy::too_many_arguments)]
    pub fn sprite_on(&mut self, page: u32, cx: f32, cy: f32, half: [f32; 2], uv: [u16; 4], color: [u8; 4], angle: f32) {
        self.push(Inst { center: [cx, cy], half, color, kind: KIND_SPRITE, p0: angle, p1: page, uv });
    }

    /// A rectangle with round corners.
    pub fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color: [u8; 4]) {
        self.push(Inst {
            center: [x + w * 0.5, y + h * 0.5],
            half: [w * 0.5, h * 0.5],
            color,
            kind: KIND_RRECT,
            p0: radius.min(w * 0.5).min(h * 0.5).max(0.0),
            p1: 0,
            uv: [0; 4],
        });
    }

    /// The outline of a rectangle with round corners, `width` pixels thick,
    /// drawn inwards from the edge.
    #[allow(clippy::too_many_arguments)]
    pub fn frame(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, width: f32, color: [u8; 4]) {
        self.push(Inst {
            center: [x + w * 0.5, y + h * 0.5],
            half: [w * 0.5, h * 0.5],
            color,
            kind: KIND_RRECT,
            p0: radius.min(w * 0.5).min(h * 0.5).max(0.0),
            p1: ((width * 16.0) as u32).max(1),
            uv: [0; 4],
        });
    }

    pub fn ring(&mut self, cx: f32, cy: f32, r: f32, width: f32, color: [u8; 4], flags: u32) {
        self.push(Inst {
            center: [cx, cy],
            half: [r, r],
            color,
            kind: KIND_RING | flags,
            p0: width,
            p1: 0,
            uv: [0; 4],
        });
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Globals {
    screen: [f32; 2],
    cursor: [f32; 2],
}

const SLOTS: usize = 2;
// Also the number of depth layers (shader.wgsl: MAX_INSTANCES).
const MAX_INSTANCES: usize = 16_384;
/// Depth formats that may be used, best first.
const DEPTH_FORMATS: [vk::Format; 3] =
    [vk::Format::D32_SFLOAT, vk::Format::X8_D24_UNORM_PACK32, vk::Format::D16_UNORM];

struct MappedBuffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut c_void,
}

/// A sampled RGBA image on the GPU.
struct Texture {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    width: u32,
    height: u32,
}

struct Slot {
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    image_available: vk::Semaphore,
    instances: MappedBuffer,
    batch: Batch,
}

pub struct Gfx {
    _entry: ash::Entry,
    instance: ash::Instance,
    surface_loader: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    pdev: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    swapchain_loader: ash::khr::swapchain::Device,

    swapchain: vk::SwapchainKHR,
    format: vk::SurfaceFormatKHR,
    extent: vk::Extent2D,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    /// Depth keeps the drawing order and lets slider body segments overlap
    /// without seams: (image, memory, view), sized like the swapchain.
    depth_format: vk::Format,
    depth: Option<(vk::Image, vk::DeviceMemory, vk::ImageView)>,
    render_done: Vec<vk::Semaphore>,
    modes: Vec<PresentMode>,
    mode: PresentMode,
    want_extent: (u32, u32),
    needs_recreate: bool,

    pool: vk::CommandPool,
    set_layout: vk::DescriptorSetLayout,
    desc_pool: vk::DescriptorPool,
    desc_set: vk::DescriptorSet,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    trail_pipeline: vk::Pipeline,
    globals: MappedBuffer,
    slots: [Slot; SLOTS],
    slot: usize,
    image_index: u32,
    /// Whether the other slot has a submission whose fence is still pending.
    prev_in_flight: bool,
    device_name: String,
    /// Indexed by `PAGE_*`.
    textures: [Texture; 3],
    atlas_sampler: vk::Sampler,
    /// Two timestamp queries per slot (start and end of the command
    /// buffer); null if the queue cannot write timestamps.
    query_pool: vk::QueryPool,
    timestamp_period_ns: f64,
    last_gpu_ns: u64,
}

impl Gfx {
    /// # Safety
    /// The handles in `surface` must stay valid for the lifetime of the
    /// returned object (drop `Gfx` before the window).
    pub unsafe fn new(
        surface: SurfaceHandle,
        size: (u32, u32),
        preferred: Option<PresentMode>,
    ) -> Result<Gfx, GfxError> {
        let entry = ash::Entry::load().map_err(|e| GfxError(format!("no Vulkan loader: {e}")))?;
        let app = vk::ApplicationInfo::default().application_name(c"rsu").api_version(vk::API_VERSION_1_3);
        let platform_ext = match surface {
            SurfaceHandle::Wayland { .. } => ash::khr::wayland_surface::NAME,
            SurfaceHandle::Xcb { .. } => ash::khr::xcb_surface::NAME,
        };
        let exts = [ash::khr::surface::NAME.as_ptr(), platform_ext.as_ptr()];
        let instance = entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app).enabled_extension_names(&exts),
            None,
        )?;
        let surface_loader = ash::khr::surface::Instance::new(&entry, &instance);
        let surface = match surface {
            SurfaceHandle::Wayland { display, surface } => ash::khr::wayland_surface::Instance::new(&entry, &instance)
                .create_wayland_surface(
                    &vk::WaylandSurfaceCreateInfoKHR::default().display(display.cast()).surface(surface.cast()),
                    None,
                )?,
            SurfaceHandle::Xcb { connection, window } => ash::khr::xcb_surface::Instance::new(&entry, &instance)
                .create_xcb_surface(
                    &vk::XcbSurfaceCreateInfoKHR::default().connection(connection.cast()).window(window),
                    None,
                )?,
        };

        // First device that can draw to this surface; discrete GPUs win.
        let mut pick: Option<(vk::PhysicalDevice, u32, bool)> = None;
        for pd in instance.enumerate_physical_devices()? {
            let props = instance.get_physical_device_properties(pd);
            if props.api_version < vk::API_VERSION_1_3 {
                continue;
            }
            let family =
                instance.get_physical_device_queue_family_properties(pd).iter().enumerate().position(|(i, q)| {
                    q.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                        && surface_loader.get_physical_device_surface_support(pd, i as u32, surface).unwrap_or(false)
                });
            if let Some(f) = family {
                let discrete = props.device_type == vk::PhysicalDeviceType::DISCRETE_GPU;
                if pick.is_none() || (discrete && !pick.unwrap().2) {
                    pick = Some((pd, f as u32, discrete));
                }
            }
        }
        let Some((pdev, family, _)) = pick else {
            return err("no Vulkan 1.3 device can present to this window");
        };
        let timestamp_bits =
            instance.get_physical_device_queue_family_properties(pdev)[family as usize].timestamp_valid_bits;
        let timestamp_period_ns = instance.get_physical_device_properties(pdev).limits.timestamp_period as f64;
        let device_name = instance
            .get_physical_device_properties(pdev)
            .device_name_as_c_str()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();

        let queue_info = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&[1.0])];
        let dev_exts = [ash::khr::swapchain::NAME.as_ptr()];
        let mut f13 = vk::PhysicalDeviceVulkan13Features::default().dynamic_rendering(true);
        let device = instance.create_device(
            pdev,
            &vk::DeviceCreateInfo::default()
                .queue_create_infos(&queue_info)
                .enabled_extension_names(&dev_exts)
                .push_next(&mut f13),
            None,
        )?;
        let queue = device.get_device_queue(family, 0);
        let swapchain_loader = ash::khr::swapchain::Device::new(&instance, &device);

        // 8-bit UNORM: colours are authored as display values, no implicit
        // sRGB conversion.
        let formats = surface_loader.get_physical_device_surface_formats(pdev, surface)?;
        let format = formats
            .iter()
            .copied()
            .find(|f| f.format == vk::Format::B8G8R8A8_UNORM)
            .or_else(|| formats.iter().copied().find(|f| f.format == vk::Format::R8G8B8A8_UNORM))
            .or_else(|| formats.first().copied())
            .ok_or_else(|| GfxError("surface has no formats".into()))?;

        let vk_modes = surface_loader.get_physical_device_surface_present_modes(pdev, surface)?;
        let modes: Vec<PresentMode> =
            PresentMode::BY_LATENCY.into_iter().filter(|m| vk_modes.contains(&m.vk())).collect();
        let mode = preferred.filter(|m| modes.contains(m)).or(modes.first().copied()).unwrap_or(PresentMode::Fifo);

        let pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )?;
        let cmds = device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(SLOTS as u32),
        )?;

        let globals = create_mapped(
            &instance,
            &device,
            pdev,
            std::mem::size_of::<Globals>() as u64,
            vk::BufferUsageFlags::UNIFORM_BUFFER,
        )?;
        let make_slot = |cmd| -> Result<Slot, GfxError> {
            let instances = create_mapped(
                &instance,
                &device,
                pdev,
                (MAX_INSTANCES * std::mem::size_of::<Inst>()) as u64,
                vk::BufferUsageFlags::VERTEX_BUFFER,
            )?;
            Ok(Slot {
                cmd,
                fence: device
                    .create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None)?,
                image_available: device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?,
                batch: unsafe { Batch::over(instances.ptr.cast(), MAX_INSTANCES) },
                instances,
            })
        };
        let slots = [make_slot(cmds[0])?, make_slot(cmds[1])?];

        // The atlases and the background: RGBA images, sampled with linear
        // filtering.
        let textures = [
            create_texture(&instance, &device, pdev, ATLAS_SIZE, ATLAS_SIZE)?,
            create_texture(&instance, &device, pdev, ATLAS_SIZE, ATLAS_SIZE)?,
            create_texture(&instance, &device, pdev, BACKGROUND_SIZE.0, BACKGROUND_SIZE.1)?,
        ];
        let atlas_sampler = device.create_sampler(
            &vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::LINEAR)
                .min_filter(vk::Filter::LINEAR)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE),
            None,
        )?;

        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::VERTEX),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(3)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(4)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let set_layout = device
            .create_descriptor_set_layout(&vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings), None)?;
        let pool_sizes = [
            vk::DescriptorPoolSize::default().ty(vk::DescriptorType::UNIFORM_BUFFER).descriptor_count(1),
            vk::DescriptorPoolSize::default().ty(vk::DescriptorType::SAMPLED_IMAGE).descriptor_count(3),
            vk::DescriptorPoolSize::default().ty(vk::DescriptorType::SAMPLER).descriptor_count(1),
        ];
        let desc_pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default().max_sets(1).pool_sizes(&pool_sizes),
            None,
        )?;
        let set_layouts = [set_layout];
        let desc_set = device.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default().descriptor_pool(desc_pool).set_layouts(&set_layouts),
        )?[0];
        let buf_info = [vk::DescriptorBufferInfo::default().buffer(globals.buffer).range(vk::WHOLE_SIZE)];
        let image_info = |t: &Texture| {
            [vk::DescriptorImageInfo::default()
                .image_view(t.view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
        };
        let image_infos = [image_info(&textures[0]), image_info(&textures[1]), image_info(&textures[2])];
        let sampler_info = [vk::DescriptorImageInfo::default().sampler(atlas_sampler)];
        device.update_descriptor_sets(
            &[
                vk::WriteDescriptorSet::default()
                    .dst_set(desc_set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .buffer_info(&buf_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(desc_set)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&image_infos[0]),
                vk::WriteDescriptorSet::default()
                    .dst_set(desc_set)
                    .dst_binding(3)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&image_infos[1]),
                vk::WriteDescriptorSet::default()
                    .dst_set(desc_set)
                    .dst_binding(4)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&image_infos[2]),
                vk::WriteDescriptorSet::default()
                    .dst_set(desc_set)
                    .dst_binding(2)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(&sampler_info),
            ],
            &[],
        );
        let layout =
            device.create_pipeline_layout(&vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts), None)?;
        // Built once, up front: no pipeline is ever compiled during play.
        let Some(depth_format) = DEPTH_FORMATS.into_iter().find(|&f| {
            instance
                .get_physical_device_format_properties(pdev, f)
                .optimal_tiling_features
                .contains(vk::FormatFeatureFlags::DEPTH_STENCIL_ATTACHMENT)
        }) else {
            return err("no depth format");
        };
        let pipeline = create_pipeline(&device, layout, format.format, depth_format, true)?;
        let trail_pipeline = match create_pipeline(&device, layout, format.format, depth_format, false) {
            Ok(pipeline) => pipeline,
            Err(error) => {
                device.destroy_pipeline(pipeline, None);
                return Err(error);
            }
        };

        let query_pool = if timestamp_bits == 64 {
            device.create_query_pool(
                &vk::QueryPoolCreateInfo::default().query_type(vk::QueryType::TIMESTAMP).query_count(SLOTS as u32 * 2),
                None,
            )?
        } else {
            vk::QueryPool::null()
        };

        let mut gfx = Gfx {
            _entry: entry,
            instance,
            surface_loader,
            surface,
            pdev,
            device,
            queue,
            swapchain_loader,
            swapchain: vk::SwapchainKHR::null(),
            format,
            extent: vk::Extent2D::default(),
            images: Vec::new(),
            views: Vec::new(),
            depth_format,
            depth: None,
            render_done: Vec::new(),
            modes,
            mode,
            want_extent: size,
            needs_recreate: false,
            pool,
            set_layout,
            desc_pool,
            desc_set,
            layout,
            pipeline,
            trail_pipeline,
            globals,
            slots,
            slot: 0,
            image_index: 0,
            prev_in_flight: false,
            device_name,
            textures,
            atlas_sampler,
            query_pool,
            timestamp_period_ns,
            last_gpu_ns: 0,
        };
        gfx.create_swapchain()?;
        // Until something is uploaded the textures only have to be in a
        // layout that may be sampled; nothing textured is drawn from them.
        for page in [PAGE_ATLAS, PAGE_UI, PAGE_BACKGROUND] {
            gfx.upload(page, 0, 0, &[])?;
        }
        Ok(gfx)
    }

    /// Replace the top `rows` rows of the atlas with `rgba` (ATLAS_SIZE
    /// pixels wide, 4 bytes per pixel). Waits for the GPU; for load time,
    /// not for frames.
    pub fn upload_atlas(&mut self, rows: u32, rgba: &[u8]) -> Result<(), GfxError> {
        self.upload(PAGE_ATLAS, ATLAS_SIZE, rows, rgba)
    }

    /// Replace the top-left `width` x `rows` pixels of texture `page` with
    /// `rgba` (premultiplied, 4 bytes per pixel, `width` pixels per row);
    /// what the texture held outside of that is not kept. Waits for the
    /// GPU; for load time, not for frames.
    pub fn upload(&mut self, page: u32, width: u32, rows: u32, rgba: &[u8]) -> Result<(), GfxError> {
        let texture = &self.textures[page as usize % 3];
        let (width, rows) = (width.min(texture.width), rows.min(texture.height));
        let bytes = (rows * width * 4) as usize;
        if rgba.len() < bytes {
            return err("texture data is shorter than the size it claims");
        }
        let image = texture.image;
        // SAFETY: all handles are ours; the device is idle while the
        // image is rewritten, and the staging buffer outlives the copy.
        unsafe {
            let d = &self.device;
            d.device_wait_idle()?;
            self.prev_in_flight = false;
            let staging = if bytes > 0 {
                let b = create_mapped(&self.instance, d, self.pdev, bytes as u64, vk::BufferUsageFlags::TRANSFER_SRC)?;
                std::ptr::copy_nonoverlapping(rgba.as_ptr(), b.ptr.cast::<u8>(), bytes);
                Some(b)
            } else {
                None
            };
            let cb = d.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0];
            d.begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            let barrier = |old, new, src, dst| {
                vk::ImageMemoryBarrier::default()
                    .old_layout(old)
                    .new_layout(new)
                    .src_access_mask(src)
                    .dst_access_mask(dst)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(COLOR_RANGE)
            };
            // Old contents are not kept: every upload rewrites what is used.
            d.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_WRITE,
                )],
            );
            if let Some(b) = &staging {
                d.cmd_copy_buffer_to_image(
                    cb,
                    b.buffer,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[vk::BufferImageCopy::default()
                        .image_subresource(vk::ImageSubresourceLayers {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            mip_level: 0,
                            base_array_layer: 0,
                            layer_count: 1,
                        })
                        .image_extent(vk::Extent3D { width, height: rows, depth: 1 })],
                );
            }
            d.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::SHADER_READ,
                )],
            );
            d.end_command_buffer(cb)?;
            d.queue_submit(self.queue, &[vk::SubmitInfo::default().command_buffers(&[cb])], vk::Fence::null())?;
            d.queue_wait_idle(self.queue)?;
            d.free_command_buffers(self.pool, &[cb]);
            if let Some(b) = staging {
                d.destroy_buffer(b.buffer, None);
                d.free_memory(b.memory, None);
            }
        }
        Ok(())
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// GPU execution time of the most recently completed frame, measured by
    /// the GPU itself between the first and last command. Time a frame spent
    /// queued before the GPU picked it up is *not* included, which is what
    /// makes this comparable against the CPU-side wait. 0 if unsupported.
    pub fn last_gpu_ns(&self) -> u64 {
        self.last_gpu_ns
    }

    fn read_gpu_time(&mut self, slot: usize) {
        if self.query_pool == vk::QueryPool::null() {
            return;
        }
        let mut t = [0u64; 2];
        // SAFETY: the slot's frame has completed, so both queries are available.
        let ok = unsafe {
            self.device.get_query_pool_results(self.query_pool, slot as u32 * 2, &mut t, vk::QueryResultFlags::TYPE_64)
        };
        if ok.is_ok() {
            self.last_gpu_ns = (t[1].wrapping_sub(t[0]) as f64 * self.timestamp_period_ns) as u64;
        }
    }

    pub fn present_mode(&self) -> PresentMode {
        self.mode
    }

    /// Modes this surface offers, lowest latency first.
    pub fn available_modes(&self) -> &[PresentMode] {
        &self.modes
    }

    pub fn set_present_mode(&mut self, mode: PresentMode) {
        if self.modes.contains(&mode) && mode != self.mode {
            self.mode = mode;
            self.needs_recreate = true;
        }
    }

    /// Tell the renderer the window's new size (needed on Wayland, where the
    /// surface has no size of its own).
    pub fn resize(&mut self, w: u32, h: u32) {
        if (w, h) != self.want_extent {
            self.want_extent = (w, h);
            self.needs_recreate = true;
        }
    }

    pub fn extent(&self) -> (u32, u32) {
        (self.extent.width, self.extent.height)
    }

    fn create_swapchain(&mut self) -> Result<(), GfxError> {
        // SAFETY: all handles belong to this object; the device is idle
        // before anything is destroyed.
        unsafe {
            self.device.device_wait_idle()?;
            self.prev_in_flight = false;
            let caps = self.surface_loader.get_physical_device_surface_capabilities(self.pdev, self.surface)?;
            let extent = if caps.current_extent.width == u32::MAX {
                vk::Extent2D {
                    width: self.want_extent.0.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                    height: self.want_extent.1.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
                }
            } else {
                caps.current_extent
            };
            for v in self.views.drain(..) {
                self.device.destroy_image_view(v, None);
            }
            for s in self.render_done.drain(..) {
                self.device.destroy_semaphore(s, None);
            }
            if let Some((image, memory, view)) = self.depth.take() {
                self.device.destroy_image_view(view, None);
                self.device.destroy_image(image, None);
                self.device.free_memory(memory, None);
            }
            self.images.clear();
            self.extent = extent;
            if extent.width == 0 || extent.height == 0 {
                return Ok(()); // minimised: try again on the next frame
            }
            // A FIFO queue is latency: keep it as short as the driver
            // allows. The other modes never queue, and one spare image
            // keeps acquire from waiting on the one being scanned out.
            let mut count = caps.min_image_count + u32::from(!self.mode.is_vsynced());
            if caps.max_image_count > 0 {
                count = count.min(caps.max_image_count);
            }
            let alpha = [vk::CompositeAlphaFlagsKHR::OPAQUE, vk::CompositeAlphaFlagsKHR::INHERIT]
                .into_iter()
                .find(|a| caps.supported_composite_alpha.contains(*a))
                .unwrap_or(vk::CompositeAlphaFlagsKHR::OPAQUE);
            let old = self.swapchain;
            let info = vk::SwapchainCreateInfoKHR::default()
                .surface(self.surface)
                .min_image_count(count)
                .image_format(self.format.format)
                .image_color_space(self.format.color_space)
                .image_extent(extent)
                .image_array_layers(1)
                .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                .pre_transform(caps.current_transform)
                .composite_alpha(alpha)
                .present_mode(self.mode.vk())
                .clipped(true)
                .old_swapchain(old);
            self.swapchain = self.swapchain_loader.create_swapchain(&info, None)?;
            if old != vk::SwapchainKHR::null() {
                self.swapchain_loader.destroy_swapchain(old, None);
            }
            self.images = self.swapchain_loader.get_swapchain_images(self.swapchain)?;
            for &image in &self.images {
                let view = self.device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(self.format.format)
                        .subresource_range(COLOR_RANGE),
                    None,
                )?;
                self.views.push(view);
                self.render_done.push(self.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?);
            }
            self.depth = Some(create_depth(&self.instance, &self.device, self.pdev, self.depth_format, extent)?);
            self.needs_recreate = false;
        }
        Ok(())
    }

    /// Step 1: get the image to draw into. This is where a vsynced mode
    /// blocks, so it comes before any input is looked at. Returns false when
    /// there is nothing to draw to this frame (resized, minimised).
    pub fn acquire(&mut self) -> Result<bool, GfxError> {
        if self.needs_recreate || self.images.is_empty() {
            self.create_swapchain()?;
            if self.images.is_empty() {
                return Ok(false);
            }
        }
        let slot = &mut self.slots[self.slot];
        // SAFETY: swapchain and semaphore are valid; the semaphore is
        // unsignalled because the slot's previous submit consumed it.
        let r = unsafe {
            self.swapchain_loader.acquire_next_image(self.swapchain, u64::MAX, slot.image_available, vk::Fence::null())
        };
        match r {
            Ok((index, suboptimal)) => {
                self.image_index = index;
                self.needs_recreate |= suboptimal;
                slot.batch.len = 0;
                slot.batch.clips.clear();
                Ok(true)
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.needs_recreate = true;
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Step 2: the instance arena of the frame being built.
    pub fn batch(&mut self) -> &mut Batch {
        &mut self.slots[self.slot].batch
    }

    /// Step 3: record the frame's command buffer.
    pub fn record(&mut self, clear: [f32; 4]) -> Result<(), GfxError> {
        let slot = &self.slots[self.slot];
        let (d, cb) = (&self.device, slot.cmd);
        let image = self.images[self.image_index as usize];
        let area = vk::Rect2D { offset: vk::Offset2D::default(), extent: self.extent };
        // SAFETY: the slot's command buffer is not pending (its fence was
        // waited on before the slot came round again).
        unsafe {
            d.begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            if self.query_pool != vk::QueryPool::null() {
                let first = self.slot as u32 * 2;
                d.cmd_reset_query_pool(cb, self.query_pool, first, 2);
                d.cmd_write_timestamp(cb, vk::PipelineStageFlags::TOP_OF_PIPE, self.query_pool, first);
            }
            // Source stage matches the submit's semaphore wait stage, so the
            // transition happens only once the image is really ours.
            let to_draw = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(COLOR_RANGE);
            d.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_draw],
            );
            let Some((depth_image, _, depth_view)) = self.depth else {
                return err("no depth buffer");
            };
            // The depth buffer is cleared every frame, so its old contents
            // can be thrown away; only the previous frame's writes must be
            // finished.
            let depth_to_draw = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
                .src_access_mask(vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE)
                .dst_access_mask(
                    vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_READ | vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE,
                )
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(depth_image)
                .subresource_range(DEPTH_RANGE);
            d.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
                vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[depth_to_draw],
            );
            let depth_attachment = vk::RenderingAttachmentInfo::default()
                .image_view(depth_view)
                .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::DONT_CARE)
                .clear_value(vk::ClearValue { depth_stencil: vk::ClearDepthStencilValue { depth: 0.0, stencil: 0 } });
            let attachments = [vk::RenderingAttachmentInfo::default()
                .image_view(self.views[self.image_index as usize])
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue { color: vk::ClearColorValue { float32: clear } })];
            d.cmd_begin_rendering(
                cb,
                &vk::RenderingInfo::default()
                    .render_area(area)
                    .layer_count(1)
                    .color_attachments(&attachments)
                    .depth_attachment(&depth_attachment),
            );
            d.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            d.cmd_set_viewport(
                cb,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: self.extent.width as f32,
                    height: self.extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            d.cmd_set_scissor(cb, 0, &[area]);
            d.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::GRAPHICS, self.layout, 0, &[self.desc_set], &[]);
            d.cmd_bind_vertex_buffers(cb, 0, &[slot.instances.buffer], &[0]);
            // Translucent trail quads must blend in instance order without
            // writing depth, or their transparent areas erase older stamps.
            let insts = std::slice::from_raw_parts(slot.batch.ptr, slot.batch.len);
            let draw_range = |range: std::ops::Range<usize>| {
                let base = range.start;
                draw_runs(&insts[range], |trail, first, count| {
                    d.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, if trail { self.trail_pipeline } else { self.pipeline });
                    d.cmd_draw(cb, 6, count as u32, 0, (base + first) as u32);
                });
            };
            let mut next = 0;
            for (range, [left, top, right, bottom]) in &slot.batch.clips {
                draw_range(next..range.start);
                // Include fragments whose pixel centers lie inside the rect.
                let edge = |v: f32, limit: u32| ((v - 0.5).ceil() as u32).min(limit);
                let x = edge(*left, self.extent.width);
                let y = edge(*top, self.extent.height);
                let w = edge(*right, self.extent.width).saturating_sub(x);
                let h = edge(*bottom, self.extent.height).saturating_sub(y);
                if w != 0 && h != 0 {
                    let scissor = vk::Rect2D {
                        offset: vk::Offset2D { x: x as i32, y: y as i32 },
                        extent: vk::Extent2D { width: w, height: h },
                    };
                    d.cmd_set_scissor(cb, 0, &[scissor]);
                    draw_range(range.clone());
                    d.cmd_set_scissor(cb, 0, &[area]);
                }
                next = range.end;
            }
            draw_range(next..insts.len());
            d.cmd_end_rendering(cb);
            let to_present = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(COLOR_RANGE);
            d.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_present],
            );
            if self.query_pool != vk::QueryPool::null() {
                d.cmd_write_timestamp(
                    cb,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    self.query_pool,
                    self.slot as u32 * 2 + 1,
                );
            }
            d.end_command_buffer(cb)?;
        }
        Ok(())
    }

    /// Step 4: wait until the previous frame has left the GPU. With this the
    /// CPU can never run ahead and build up a queue of stale frames, and the
    /// shared uniform buffer is safe to overwrite.
    pub fn wait_previous(&mut self) -> Result<(), GfxError> {
        if self.prev_in_flight {
            let prev = &self.slots[(self.slot + SLOTS - 1) % SLOTS];
            // SAFETY: the fence is valid and was submitted with a batch.
            unsafe { self.device.wait_for_fences(&[prev.fence], true, u64::MAX)? };
            self.read_gpu_time((self.slot + SLOTS - 1) % SLOTS);
            self.prev_in_flight = false;
        }
        Ok(())
    }

    /// Step 5: the late latch. Call after `wait_previous`, immediately
    /// before `submit_present`.
    #[inline]
    pub fn latch_cursor(&mut self, x: f32, y: f32) {
        let g = Globals { screen: [self.extent.width as f32, self.extent.height as f32], cursor: [x, y] };
        // SAFETY: ptr maps a buffer of exactly size_of::<Globals>() and no
        // submitted frame is still reading it.
        unsafe { self.globals.ptr.cast::<Globals>().write_volatile(g) };
    }

    /// Step 6: hand the frame to the GPU and the presentation engine.
    pub fn submit_present(&mut self) -> Result<(), GfxError> {
        let slot = &self.slots[self.slot];
        let done = [self.render_done[self.image_index as usize]];
        let wait = [slot.image_available];
        let cmds = [slot.cmd];
        let swapchains = [self.swapchain];
        let indices = [self.image_index];
        // SAFETY: every handle is valid; the fence is signalled (never used,
        // or waited on by `wait_previous` a frame ago) so it can be reset.
        let presented = unsafe {
            self.device.reset_fences(&[slot.fence])?;
            self.device.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default()
                    .wait_semaphores(&wait)
                    .wait_dst_stage_mask(&[vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT])
                    .command_buffers(&cmds)
                    .signal_semaphores(&done)],
                slot.fence,
            )?;
            self.swapchain_loader.queue_present(
                self.queue,
                &vk::PresentInfoKHR::default().wait_semaphores(&done).swapchains(&swapchains).image_indices(&indices),
            )
        };
        self.prev_in_flight = true;
        self.slot = (self.slot + 1) % SLOTS;
        match presented {
            Ok(suboptimal) => self.needs_recreate |= suboptimal,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.needs_recreate = true,
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
}

impl Drop for Gfx {
    fn drop(&mut self) {
        // SAFETY: we own every handle; the device is idle before teardown.
        unsafe {
            let d = &self.device;
            let _ = d.device_wait_idle();
            for v in self.views.drain(..) {
                d.destroy_image_view(v, None);
            }
            for s in self.render_done.drain(..) {
                d.destroy_semaphore(s, None);
            }
            if let Some((image, memory, view)) = self.depth.take() {
                d.destroy_image_view(view, None);
                d.destroy_image(image, None);
                d.free_memory(memory, None);
            }
            if self.swapchain != vk::SwapchainKHR::null() {
                self.swapchain_loader.destroy_swapchain(self.swapchain, None);
            }
            for s in &self.slots {
                d.destroy_fence(s.fence, None);
                d.destroy_semaphore(s.image_available, None);
                d.destroy_buffer(s.instances.buffer, None);
                d.free_memory(s.instances.memory, None);
            }
            if self.query_pool != vk::QueryPool::null() {
                d.destroy_query_pool(self.query_pool, None);
            }
            d.destroy_sampler(self.atlas_sampler, None);
            for t in &self.textures {
                d.destroy_image_view(t.view, None);
                d.destroy_image(t.image, None);
                d.free_memory(t.memory, None);
            }
            d.destroy_buffer(self.globals.buffer, None);
            d.free_memory(self.globals.memory, None);
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline(self.trail_pipeline, None);
            d.destroy_pipeline_layout(self.layout, None);
            d.destroy_descriptor_pool(self.desc_pool, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
            d.destroy_command_pool(self.pool, None);
            d.destroy_device(None);
            self.surface_loader.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
}

const COLOR_RANGE: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
    aspect_mask: vk::ImageAspectFlags::COLOR,
    base_mip_level: 0,
    level_count: 1,
    base_array_layer: 0,
    layer_count: 1,
};

const DEPTH_RANGE: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
    aspect_mask: vk::ImageAspectFlags::DEPTH,
    base_mip_level: 0,
    level_count: 1,
    base_array_layer: 0,
    layer_count: 1,
};

unsafe fn create_depth(
    instance: &ash::Instance,
    device: &ash::Device,
    pdev: vk::PhysicalDevice,
    format: vk::Format,
    extent: vk::Extent2D,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView), GfxError> {
    let image = device.create_image(
        &vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D { width: extent.width, height: extent.height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT)
            .sharing_mode(vk::SharingMode::EXCLUSIVE),
        None,
    )?;
    let req = device.get_image_memory_requirements(image);
    let mem = instance.get_physical_device_memory_properties(pdev);
    let Some(ty) = (0..mem.memory_type_count).find(|&i| {
        req.memory_type_bits & (1 << i) != 0
            && mem.memory_types[i as usize].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
    }) else {
        device.destroy_image(image, None);
        return err("no device memory type for the depth buffer");
    };
    let memory = device
        .allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None)?;
    device.bind_image_memory(image, memory, 0)?;
    let view = device.create_image_view(
        &vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(DEPTH_RANGE),
        None,
    )?;
    Ok((image, memory, view))
}

/// A host-visible buffer mapped for its whole life: per-frame data is written
/// directly, with no staging copies and no allocation.
unsafe fn create_mapped(
    instance: &ash::Instance,
    device: &ash::Device,
    pdev: vk::PhysicalDevice,
    size: u64,
    usage: vk::BufferUsageFlags,
) -> Result<MappedBuffer, GfxError> {
    let buffer = device.create_buffer(
        &vk::BufferCreateInfo::default().size(size).usage(usage).sharing_mode(vk::SharingMode::EXCLUSIVE),
        None,
    )?;
    let req = device.get_buffer_memory_requirements(buffer);
    let mem = instance.get_physical_device_memory_properties(pdev);
    let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let Some(ty) = (0..mem.memory_type_count)
        .find(|&i| req.memory_type_bits & (1 << i) != 0 && mem.memory_types[i as usize].property_flags.contains(want))
    else {
        return err("no host-visible memory type");
    };
    let memory = device
        .allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None)?;
    device.bind_buffer_memory(buffer, memory, 0)?;
    let ptr = device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())?;
    Ok(MappedBuffer { buffer, memory, ptr })
}

unsafe fn create_texture(
    instance: &ash::Instance,
    device: &ash::Device,
    pdev: vk::PhysicalDevice,
    width: u32,
    height: u32,
) -> Result<Texture, GfxError> {
    let image = device.create_image(
        &vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE),
        None,
    )?;
    let req = device.get_image_memory_requirements(image);
    let mem = instance.get_physical_device_memory_properties(pdev);
    let Some(ty) = (0..mem.memory_type_count).find(|&i| {
        req.memory_type_bits & (1 << i) != 0
            && mem.memory_types[i as usize].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
    }) else {
        return err("no device memory type for a texture");
    };
    let memory = device
        .allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None)?;
    device.bind_image_memory(image, memory, 0)?;
    let view = device.create_image_view(
        &vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .subresource_range(COLOR_RANGE),
        None,
    )?;
    Ok(Texture { image, memory, view, width, height })
}

unsafe fn create_pipeline(
    device: &ash::Device,
    layout: vk::PipelineLayout,
    format: vk::Format,
    depth_format: vk::Format,
    depth_write: bool,
) -> Result<vk::Pipeline, GfxError> {
    let spv = include_bytes!(concat!(env!("OUT_DIR"), "/shader.spv"));
    let code = ash::util::read_spv(&mut std::io::Cursor::new(&spv[..])).map_err(|e| GfxError(e.to_string()))?;
    let module = device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)?;
    let stages = [
        vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(module).name(c"vs"),
        vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(module).name(c"fs"),
    ];
    let bindings = [vk::VertexInputBindingDescription {
        binding: 0,
        stride: std::mem::size_of::<Inst>() as u32,
        input_rate: vk::VertexInputRate::INSTANCE,
    }];
    let attr = |location, format, offset| vk::VertexInputAttributeDescription { location, binding: 0, format, offset };
    let attrs = [
        attr(0, vk::Format::R32G32_SFLOAT, 0),
        attr(1, vk::Format::R32G32_SFLOAT, 8),
        attr(2, vk::Format::R8G8B8A8_UNORM, 16),
        attr(3, vk::Format::R32_UINT, 20),
        attr(4, vk::Format::R32_SFLOAT, 24),
        attr(5, vk::Format::R32_UINT, 28),
        attr(6, vk::Format::R16G16B16A16_UNORM, 32),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings)
        .vertex_attribute_descriptions(&attrs);
    let assembly = vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let viewport = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .line_width(1.0);
    let multisample =
        vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        // Premultiplied alpha (the shader multiplies): the only kind that
        // stays correct when a texture is filtered across a transparent edge.
        .src_color_blend_factor(vk::BlendFactor::ONE)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD)
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
    // Every instance lies above the ones before it (the shader derives its
    // depth from the instance index), so depth never changes the drawing
    // order; it only decides between overlapping slider body segments.
    let depth = vk::PipelineDepthStencilStateCreateInfo::default()
        .depth_test_enable(true)
        .depth_write_enable(depth_write)
        .depth_compare_op(vk::CompareOp::GREATER_OR_EQUAL);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let formats = [format];
    let mut rendering = vk::PipelineRenderingCreateInfo::default()
        .color_attachment_formats(&formats)
        .depth_attachment_format(depth_format);
    let info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&assembly)
        .viewport_state(&viewport)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&blend)
        .depth_stencil_state(&depth)
        .dynamic_state(&dynamic)
        .layout(layout)
        .push_next(&mut rendering);
    let result = device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None);
    device.destroy_shader_module(module, None);
    match result {
        Ok(p) => Ok(p[0]),
        Err((_, e)) => Err(e.into()),
    }
}

/// Partition the instance arena into normal draws and transparent trail
/// draws. `first_instance` stays the index in the original batch so the
/// shader's depth layers and late-latched cursor still line up.
fn draw_runs(insts: &[Inst], mut draw: impl FnMut(bool, usize, usize)) {
    let mut first = 0;
    while first < insts.len() {
        let trail = insts[first].kind & FLAG_TRAIL != 0;
        let mut end = first + 1;
        while end < insts.len() && (insts[end].kind & FLAG_TRAIL != 0) == trail {
            end += 1;
        }
        draw(trail, first, end - first);
        first = end;
    }
}

#[cfg(test)]
mod trail_tests {
    use super::*;

    #[test]
    fn trail_instances_get_a_depth_read_only_draw_between_ordinary_draws() {
        let inst = |kind| Inst { center: [0.0; 2], half: [1.0; 2], color: [255; 4], kind, p0: 0.0, p1: 0, uv: [0; 4] };
        let items = [inst(KIND_RECT), inst(KIND_SPRITE | FLAG_TRAIL), inst(KIND_SPRITE | FLAG_TRAIL), inst(KIND_SPRITE)];
        let mut runs = Vec::new();
        draw_runs(&items, |read_only, start, count| runs.push((read_only, start, count)));
        assert_eq!(runs, [(false, 0, 1), (true, 1, 2), (false, 3, 1)]);
        runs.clear();
        draw_runs(&[], |read_only, start, count| runs.push((read_only, start, count)));
        assert!(runs.is_empty());
    }
}
