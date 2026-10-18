// CEF 150 для современного хоста (cef-modern): корень дистрибутива — CEF150_PATH, отдельно от
// CEF 89 (CEF_PATH, крейт cef-sys), чтобы обе сборки хоста жили в одном workspace.
fn main() {
    println!("cargo:rerun-if-env-changed=CEF150_PATH");
    println!("cargo:rerun-if-env-changed=CEF_SYS_SKIP_LINK");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_MIRI");

    if std::env::var("CEF_SYS_SKIP_LINK").is_ok() || std::env::var("CARGO_CFG_MIRI").is_ok() {
        println!("cargo:warning=Skipping libcef.lib linkage (CEF_SYS_SKIP_LINK or Miri).");
        return;
    }

    let cef_redist = std::env::var("CEF150_PATH")
        .expect("No CEF150_PATH: корень дистрибутива CEF 150 (в нём Release\\libcef.lib)");

    println!("cargo:rustc-link-search=native={}/Release", cef_redist);
    println!("cargo:rustc-link-lib=static=libcef");
}
