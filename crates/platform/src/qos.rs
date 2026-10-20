//! Thread quality of service (macOS): background work that yields to anything interactive.

/// `QOS_CLASS_UTILITY` (`<sys/qos.h>`): long-running work the user isn't waiting for, which the
/// scheduler runs after user-interactive and user-initiated threads (the UI, the window server,
/// other apps in front) and prefers to put on efficiency cores.
const QOS_CLASS_UTILITY: u32 = 0x11;

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

#[cfg(test)]
mod tests {
    #[test]
    fn a_thread_can_lower_itself_to_utility() {
        let ok = std::thread::spawn(super::utility).join().expect("thread");
        assert!(ok, "pthread_set_qos_class_self_np refused QOS_CLASS_UTILITY");
    }
}
