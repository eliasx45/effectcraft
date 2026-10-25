//! Thread quality of service (macOS): background work that yields to anything interactive.

/// `QOS_CLASS_UTILITY` (`<sys/qos.h>`): long-running work the user isn't waiting for, which the
/// scheduler runs after user-interactive and user-initiated threads (the UI, the window server,
/// other apps in front) and prefers to put on efficiency cores.
const QOS_CLASS_UTILITY: u32 = 0x11;

/// `QOS_CLASS_USER_INITIATED`: work the user started and waits for (the viewer's frame,
/// playback), after the UI itself.
const QOS_CLASS_USER_INITIATED: u32 = 0x19;

// SAFETY: `pthread_set_qos_class_self_np` (`<pthread/qos.h>`, libSystem) takes a QoS class and a
// relative priority by value and only changes the calling thread's scheduling; invalid values
// make it return an error code (EINVAL), never undefined behaviour, so it is declared `safe`.
unsafe extern "C" {
    safe fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
}

/// Lower the calling thread to utility QoS. Whether the system accepted it.
pub fn utility() -> bool {
    pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0) == 0
}

/// Raise the calling thread to user-initiated QoS (after [`utility`]). Whether the system
/// accepted it.
pub fn user_initiated() -> bool {
    pthread_set_qos_class_self_np(QOS_CLASS_USER_INITIATED, 0) == 0
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_thread_can_lower_itself_to_utility() {
        let ok = std::thread::spawn(super::utility).join().expect("thread");
        assert!(ok, "pthread_set_qos_class_self_np refused QOS_CLASS_UTILITY");
    }

    /// A frame worker goes down for prefetch and back up for the viewer's frame.
    #[test]
    fn a_thread_can_go_down_and_back_up() {
        let ok = std::thread::spawn(|| super::utility() && super::user_initiated() && super::utility()).join().expect("thread");
        assert!(ok, "pthread_set_qos_class_self_np refused a change");
    }
}
