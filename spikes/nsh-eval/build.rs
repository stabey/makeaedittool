fn main() {
    println!("cargo:rerun-if-changed=app/App.tsx");
    #[cfg(feature = "panel")]
    {
        let entry = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("app/App.tsx");
        let sets = vec![nsh_core::element::description::SetDescription::from(nsh_base::registry())];
        let bundle = nsh_bundler::bundle(&nsh_bundler::BundleOptions {
            entry, minify: true, split: false, ui_sets: sets,
        }).unwrap_or_else(|error| panic!("TSX bundling failed: {}", error.plain));
        let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("panel.js");
        std::fs::write(output, bundle.code).expect("write bundled panel");
    }
}
