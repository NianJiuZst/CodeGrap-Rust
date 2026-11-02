fn main() {
    // P1: napi_build::setup() removed. P5: the Kotlin and Swift grammars
    // (vendored C) are compiled here; lua, scala and dart are not vendored.
    if std::env::var_os("CARGO_FEATURE_LANG_KOTLIN").is_some() {
        // Kotlin grammar — vendored C, compiled here instead of a crate dep: the
        // crates.io tree-sitter-kotlin 0.3.8 pins `tree-sitter >= 0.21, < 0.23`
        // (the kernel links 0.25) and tree-sitter-kotlin-ng is a DIFFERENT
        // grammar (8 fields vs 0, renamed kinds — extractor-breaking). Sources
        // are the fwcd 0.3.8 tag's checked-in generated artifacts, sha-matched
        // against the crates.io tarball (kotlin checklist §Grammar prep):
        //   parser.c  54104a7ef1555c265b746c790e0f8bb953cc17806e9df0c3af82f7f62c06a70a
        //   scanner.c 27f73337ec357fc341fa57538f34c14277b0346980c3405dc30beab6202ec6d0
        // scanner.c carries docs/grammars/tree-sitter-kotlin.patch (same-line
        // `e`-word semicolon fix), shared with the wasm build:
        //   scanner.c 2ca842dd04b60b0a6df59a03e9ad01209d8c5641f079a79a84d65708669b6cb6 (patched)
        // Flags crib the tarball's own bindings/rust/build.rs.
        let mut c = cc::Build::new();
        c.include("grammars/kotlin");
        c.file("grammars/kotlin/parser.c");
        c.file("grammars/kotlin/scanner.c");
        c.flag_if_supported("-Wno-unused-parameter");
        c.flag_if_supported("-Wno-unused-but-set-variable");
        c.flag_if_supported("-Wno-trigraphs");
        c.flag_if_supported("-utf-8"); // msvc
        c.compile("tree-sitter-kotlin");
        println!("cargo:rerun-if-changed=grammars/kotlin");
    }
    // P5: Swift grammar - vendored C, the crates.io tree-sitter-swift 0.7.3
    // tarball's src/ with upstream's scanner fix (`1ULL << FAKE_TRY_BANG`: the
    // crate's `1UL` overflows where `unsigned long` is 32 bits, i.e. MSVC, and
    // `try! Foo()` would read as `!Foo`). Flags crib the tarball's own
    // bindings/rust/build.rs (c11).
    //   parser.c  d3edff6effe31b9a507f496577407987343b101b23eb7bee7a9b050e8ab5d27a
    //   scanner.c 8acafef098ea4a52ebfc4cdf2fe26706113a22ac8ba2faf7d952755ff350cad6 (patched)
    if std::env::var_os("CARGO_FEATURE_LANG_SWIFT").is_some() {
        let mut swift = cc::Build::new();
        swift.std("c11");
        swift.include("grammars/swift");
        swift.file("grammars/swift/parser.c");
        swift.file("grammars/swift/scanner.c");
        swift.flag_if_supported("-Wno-unused-parameter");
        swift.flag_if_supported("-Wno-unused-but-set-variable");
        swift.flag_if_supported("-Wno-trigraphs");
        swift.flag_if_supported("-utf-8"); // msvc
        swift.compile("tree-sitter-swift");
        println!("cargo:rerun-if-changed=grammars/swift");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
