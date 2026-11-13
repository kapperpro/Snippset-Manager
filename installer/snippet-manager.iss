; Inno Setup Skript (https://jrsoftware.org/isinfo.php, kostenlos)
; Vorher:  cargo build --release
; Dann:    "snippet-manager.iss" in Inno Setup öffnen und auf "Compile" klicken
;          (oder per Kommandozeile: ISCC.exe installer\snippet-manager.iss)
; Ergebnis: installer\output\SnippetManager-Setup.exe

#define AppName    "Snippet-Manager"
#define AppVersion "0.5.0"
#define AppExe     "snippet-manager.exe"

[Setup]
; Eigene, feste GUID – NIE ändern, sonst erkennt Windows Updates nicht als Update.
AppId={{B6F1A5D2-3C7E-4F0B-9A8D-5E2C1D7F4A90}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Snippet-Manager
DefaultDirName={autopf}\{#AppName}
DisableProgramGroupPage=yes
OutputDir=output
OutputBaseFilename=SnippetManager-Setup
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
