title: fix(startup): launch guard needs two early stops before OpenGL; Vulkan choice
branch: upstream/launch-guard-two-strikes
depends: none

## What
- The launch guard (#243) now needs two early-stopping launches in a row before it switches Window Graphics to OpenGL. A single killed or quickly closed launch only logs a warning. A window that fails to open with an error still switches at once, and a second copy of the app no longer overwrites the first one's marker.
- Window Graphics gains a Vulkan choice (Windows/Linux; it means Automatic on macOS). It's for machines where Automatic lands on DirectX 12, which can't build the GPU compositor (#613).

## Why
On a Windows 11 / RTX 4060 laptop, one launch that was ended from outside within 3 s of its first frame switched a working Vulkan setup to OpenGL for good. That disables the GPU compositor, so previews rendered on the CPU until Settings were reset by hand. A driver that really crashes does so on every launch, so two strikes keep #243's protection.

## How tested
New tests `one_launch_killed_early_doesnt_switch_to_opengl`, `a_window_that_failed_to_open_switches_the_next_launch` and `window_graphics_choices_pick_backends`; the existing launch_guard tests and `window_graphics_is_validated` are updated. All run on macOS and on Windows, where the OpenGL switch applies. With no override, Automatic picked Vulkan on the RTX 4060 laptop.
