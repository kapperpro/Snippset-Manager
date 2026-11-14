; Inno Setup Skript für den Snippet-Manager (https://jrsoftware.org/isinfo.php, kostenlos)
;
; 1. Im Projektordner:  cargo build --release
; 2. Diese Datei in Inno Setup öffnen und auf "Compile" klicken (Strg+F9)
;    oder:  "C:\Program Files (x86)\Inno Setup 6\ISCC.exe" installer\snippet-manager.iss
; Ergebnis: installer\output\SnippetManager-Setup-<Version>.exe
;
; Die Versionsnummer wird automatisch aus der gebauten .exe gelesen
; (sie stammt aus "version" in der Cargo.toml) – hier muss nichts angepasst werden.

#define AppName    "Snippet-Manager"
#define AppExe     "snippet-manager.exe"
#define ExePath    "..\target\release\" + AppExe

#if !FileExists(ExePath)
  #error Die Datei target\release\snippet-manager.exe fehlt. Bitte zuerst "cargo build --release" ausfuehren.
#endif

#define AppVersion GetStringFileInfo(ExePath, "ProductVersion")
#if AppVersion == ""
  ; Fallback, falls die .exe keine Versionsinfo enthaelt (build.rs-Warnung beim Bauen).
  #define AppVersion "0.7.0"
#endif

[Setup]
; Eigene, feste GUID – NIE ändern, sonst erkennt Windows Updates nicht als Update.
AppId={{B6F1A5D2-3C7E-4F0B-9A8D-5E2C1D7F4A90}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Snippet-Manager
DefaultDirName={autopf}\{#AppName}
DisableProgramGroupPage=yes
OutputDir=output
OutputBaseFilename=SnippetManager-Setup-{#AppVersion}
VersionInfoVersion={#AppVersion}
SetupIconFile=..\assets\icon.ico
UninstallDisplayIcon={app}\{#AppExe}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Standard: Installation für alle Benutzer nach C:\Program Files (Admin-Abfrage).
; Im Dialog kann der Nutzer stattdessen "nur für mich" wählen (dann %LOCALAPPDATA%\Programs).
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog

[Languages]
Name: "german"; MessagesFile: "compiler:Languages\German.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
; Beim Deinstallieren den Autostart-Eintrag der App wieder entfernen.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueName: "SnippetManager"; ValueType: none; Flags: uninsdeletevalue

[Run]
Filename: "{app}\{#AppExe}"; Description: "{#AppName} jetzt starten"; Flags: nowait postinstall skipifsilent

[Code]
// Die App läuft evtl. unsichtbar im Tray und blockiert dann das Überschreiben
// bzw. Löschen der .exe. Deshalb wird sie vor Installation/Deinstallation beendet.
procedure StopApp();
var
  ResultCode: Integer;
begin
  Exec('taskkill.exe', '/F /IM snippet-manager.exe', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopApp();
  Result := '';
end;

function InitializeUninstall(): Boolean;
begin
  StopApp();
  Result := True;
end;
