//! LVR1: this process's video memory use and the budget the driver gives it, on Vulkan
//! (`VK_EXT_memory_budget`): the numbers wgpu's memory budget thresholds are a share of. The
//! compositor's frame gate keeps the use well below the threshold with them
//! (`effectcraft_gpu::vram::HeapBudget`); wgpu itself doesn't expose them.

use ash::vk;
use effectcraft_gpu::vram::HeapBudget;

/// The largest device-local heap's use and budget for `device`. `None` off Vulkan, without
/// `VK_EXT_memory_budget`, or on a Vulkan instance older than 1.1.
pub(crate) fn heap_budget(device: &wgpu::Device) -> Option<HeapBudget> {
    // SAFETY: the guard keeps the device (and its instance) alive while its physical device's
    // memory properties are read; no handle is created, kept or destroyed here.
    let hal = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }?;
    if !hal.enabled_device_extensions().contains(&ash::ext::memory_budget::NAME) {
        return None;
    }
    let instance = hal.shared_instance();
    // vkGetPhysicalDeviceMemoryProperties2 is core from Vulkan 1.1 (ash's stub panics otherwise).
    if instance.instance_api_version() < vk::API_VERSION_1_1 {
        return None;
    }
    let mut budget = vk::PhysicalDeviceMemoryBudgetPropertiesEXT::default();
    let memory = {
        let mut props = vk::PhysicalDeviceMemoryProperties2::default().push_next(&mut budget);
        // SAFETY: a physical device of this instance, and an output struct chained only to a
        // budget struct the extension (enabled above) defines.
        unsafe { instance.raw_instance().get_physical_device_memory_properties2(hal.raw_physical_device(), &mut props) };
        props.memory_properties
    };
    let count = usize::try_from(memory.memory_heap_count).unwrap_or(0).min(memory.memory_heaps.len());
    let heap = memory
        .memory_heaps
        .get(..count)?
        .iter()
        .enumerate()
        .filter(|(_, h)| h.flags.contains(vk::MemoryHeapFlags::DEVICE_LOCAL))
        .max_by_key(|(_, h)| h.size)
        .map(|(i, _)| i)?;
    let (usage, budget) = (*budget.heap_usage.get(heap)?, *budget.heap_budget.get(heap)?);
    (budget > 0).then_some(HeapBudget { usage, budget })
}
