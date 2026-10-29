// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
#[cfg(test)]
mod tests {
    #[test]
    fn platform_smoke() {
        let root = std::env::temp_dir().join(format!("platform-smoke-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create smoke directory");
        std::fs::write(root.join("source"), b"current-workspace").expect("write smoke data");
        std::fs::rename(root.join("source"), root.join("destination")).expect("rename smoke data");
        assert_eq!(
            std::fs::read(root.join("destination")).expect("read smoke data"),
            b"current-workspace"
        );
        std::fs::remove_dir_all(root).expect("remove generated smoke directory");
    }
}
