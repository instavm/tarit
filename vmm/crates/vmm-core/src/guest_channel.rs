//! Guest-host communication channel via guest physical memory.
//!
//! This channel provides a way for the guest's init script to pass
//! command output back to the VMM after the VM halts. It complements
//! serial output (which works via earlycon + the 8250 console on
//! port 0x3f8) by providing structured data (exit code + output).
//!
//! Protocol:
//!   GPA 0x70000: [1B ready flag][1B exit_code][254B output]
//!   - ready flag: 0x00 = not ready, 0x01 = ready (init ran)
//!   - exit_code: 0 = success
//!   - output: NUL-terminated string (command output)
//!
//! The guest's init script writes to this address via /dev/mem.

pub const CHANNEL_ADDR: u64 = 0x70000;
pub const CHANNEL_SIZE: usize = 256;
pub const READY_FLAG_OFFSET: usize = 0;
pub const EXIT_CODE_OFFSET: usize = 1;
pub const OUTPUT_OFFSET: usize = 2;
pub const OUTPUT_SIZE: usize = 254;
pub const READY_FLAG: u8 = 0x01;

/// Read the guest's readiness + output from the memory channel.
pub fn read_channel(mem: &vmm_memory_backend::GuestMemory) -> (bool, i32, String) {
    let mut buf = [0u8; CHANNEL_SIZE];
    let _ = mem.read_phys(CHANNEL_ADDR, &mut buf);

    let ready = buf[READY_FLAG_OFFSET] == READY_FLAG;
    let exit_code = buf[EXIT_CODE_OFFSET] as i32;
    let output_end = buf[OUTPUT_OFFSET..]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(OUTPUT_SIZE);
    let output =
        String::from_utf8_lossy(&buf[OUTPUT_OFFSET..OUTPUT_OFFSET + output_end]).to_string();

    (ready, exit_code, output)
}

/// Zero the channel before boot (so we can detect the guest wrote to it).
pub fn clear_channel(mem: &vmm_memory_backend::GuestMemory) {
    let _ = mem.write_phys(CHANNEL_ADDR, &[0u8; CHANNEL_SIZE]);
}

/// Build the init script that writes to the memory channel.
/// The init script:
/// 1. Writes READY_FLAG to 0x70000
/// 2. Executes the given command
/// 3. Writes the command output + exit code to the channel
/// 4. Powers off
pub fn build_init_script(command: &str) -> String {
    format!(
        r#"#!/bin/busybox sh
mount -t proc none /proc 2>/dev/null
mount -t sysfs none /sys 2>/dev/null
mount -t devtmpfs devtmpfs /dev 2>/dev/null

# Signal readiness
echo -n -e '\x01' | dd of=/dev/mem bs=1 seek=$((0x70000)) count=1 2>/dev/null

# Execute the command and capture output
OUTPUT=$({command} 2>&1)
EXIT_CODE=$?

# Write exit code
printf '\\x%02x' "$EXIT_CODE" | dd of=/dev/mem bs=1 seek=$((0x70001)) count=1 2>/dev/null

# Write output (up to 254 bytes)
echo -n "$OUTPUT" | dd of=/dev/mem bs=1 seek=$((0x70002)) count=254 2>/dev/null

# Power off
poweroff -f
"#,
        command = command
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_constants_are_consistent() {
        assert_eq!(CHANNEL_SIZE, 256);
        assert_eq!(OUTPUT_OFFSET, 2);
        assert_eq!(OUTPUT_OFFSET + OUTPUT_SIZE, CHANNEL_SIZE);
    }

    #[test]
    fn build_init_script_contains_command() {
        let script = build_init_script("echo hello");
        assert!(script.contains("echo hello"));
        assert!(script.contains("0x70000"));
        assert!(script.contains("poweroff"));
    }

    #[test]
    fn build_init_script_escapes_special_commands() {
        let script = build_init_script("node -v");
        assert!(script.contains("node -v"));
    }
}
