fn main() {
    if cfg!(target_os = "windows") {
        let mut res = winres::WindowsResource::new();
        res.set("ProductName", "Repo Debug Tool")
            .set("FileDescription", "Multi Repository Debug Tool")
            .set("LegalCopyright", "Copyright (C) 2026"); // 可根据需要修改

        // 注意：版本号会自动从 Cargo.toml 中的 version 字段读取
        res.compile().expect("Failed to compile Windows resources");
    }
}
