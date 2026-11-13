// Bettet Icon und Versionsinformationen in die .exe ein
// (sichtbar im Explorer, in den Dateieigenschaften und in der Programmliste).
// Vollständige Metadaten verbessern die Reputation bei Virenscannern.
fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Snippet-Manager");
        res.set("FileDescription", "Snippet- und Clipboard-Manager");
        res.set("InternalName", "snippet-manager");
        res.set("OriginalFilename", "snippet-manager.exe");
        res.set("CompanyName", "Snippet-Manager");
        res.set("LegalCopyright", "Copyright (c) 2026");
        if let Err(e) = res.compile() {
            // Nicht fatal: das Programm funktioniert auch ohne eingebettetes Icon.
            println!("cargo:warning=Icon konnte nicht eingebettet werden: {e}");
        }
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
}
