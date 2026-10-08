fn main() {
    // tauri-build 只对 tauri.conf.json / capabilities 声明 rerun-if-changed，
    // 图标不在其中：改图标不会重跑 build script，Windows 资源里的 exe 图标
    // 会一直停留在旧图（托盘图标走 include_bytes! 所以看起来又变了）。
    // 这里显式声明，保证图标一改就重新生成资源并重链接。
    println!("cargo:rerun-if-changed=icons");
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-changed=icons/icon.png");
    println!("cargo:rerun-if-changed=icons/128x128@2x.png");
    tauri_build::build()
}
