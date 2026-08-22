fn main() {
    #[cfg(target_os = "windows")]
    {
        embed_manifest::embed_manifest_file("cmux-cua-uia.manifest")
            .expect("failed to embed cmux-cua-uia.manifest");
        println!("cargo:rerun-if-changed=cmux-cua-uia.manifest");
    }
}
