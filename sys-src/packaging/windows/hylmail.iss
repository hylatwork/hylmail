; HylMail - Inno Setup script (per-user installation).
;
; Build (from the repository root, Inno Setup 6.1 or newer):
;   cargo build --release -p hylmail-cli
;   iscc /DHylMailVersion=0.1.0 packaging\windows\hylmail.iss
;
; Both launchers are installed: `hylmail-gui.exe` for the window (shortcut,
; file association, post-install run) and `hylmail-cli.exe` for the terminal.
; The version is passed in from `env!("CARGO_PKG_VERSION")` when the release
; pipeline runs; it is never edited by hand.
;
; The WiX script next to this file builds the *per-machine* MSI.  Both install
; the same two executables and both claim the `HylMail.eml` ProgID, so only one
; of them should ever be installed at a time.

#ifndef HylMailVersion
  #define HylMailVersion "0.0.0"
#endif

#define HylMailName        "HylMail"
#define HylMailExe         "hylmail-gui.exe"
#define HylMailCliExe      "hylmail-cli.exe"
#define HylMailPublisher   "HylMail Contributors"
#define HylMailUrl         "https://example.org/hylmail"
#define HylMailProgId      "HylMail.eml"
; Inno identifies an installation by the AppId below, so it must never change;
; the per-machine MSI uses its own UpgradeCode for the same purpose.  The value
; is written out in [Setup] rather than through a #define: a define that ends in
; a `}` confuses the preprocessor, and `{{` is what Inno wants for a literal
; brace anyway.

[Setup]
AppId={{8E0F2C6A-3A4E-4C2B-9D62-6E6B0A1D5F31}
AppName={#HylMailName}
AppVersion={#HylMailVersion}
AppVerName={#HylMailName} {#HylMailVersion}
AppPublisher={#HylMailPublisher}
AppPublisherURL={#HylMailUrl}
DefaultDirName={localappdata}\Programs\{#HylMailName}
DefaultGroupName={#HylMailName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
SetupIconFile=..\icons\hylmail.ico
OutputDir=..\..\dist
OutputBaseFilename=HylMail-{#HylMailVersion}-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\hylmail.ico
MinVersion=10.0

[Languages]
Name: "german"; MessagesFile: "compiler:Languages\German.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "..\..\target\release\{#HylMailExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\{#HylMailCliExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\icons\hylmail.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#HylMailName}"; Filename: "{app}\{#HylMailExe}"; IconFilename: "{app}\hylmail.ico"
Name: "{group}\{#HylMailName} (Befehlszeile)"; Filename: "{app}\{#HylMailCliExe}"
Name: "{group}\{cm:UninstallProgram,{#HylMailName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#HylMailName}"; Filename: "{app}\{#HylMailExe}"; IconFilename: "{app}\hylmail.ico"; \
    Tasks: desktopicon

[Registry]
; File association for .eml; the running instance is reached through IPC.
Root: HKCU; Subkey: "Software\Classes\{#HylMailProgId}"; ValueType: string; ValueName: ""; \
    ValueData: "HylMail message"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\{#HylMailProgId}\DefaultIcon"; ValueType: string; \
    ValueName: ""; ValueData: "{app}\hylmail.ico"
Root: HKCU; Subkey: "Software\Classes\{#HylMailProgId}\shell\open\command"; ValueType: string; \
    ValueName: ""; ValueData: """{app}\{#HylMailExe}"" ""%1"""
Root: HKCU; Subkey: "Software\Classes\.eml\OpenWithProgids"; ValueType: string; \
    ValueName: "{#HylMailProgId}"; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Clients\Mail\{#HylMailName}"; ValueType: string; ValueName: ""; \
    ValueData: "HylMail"; Flags: uninsdeletekey

[Run]
Filename: "{app}\{#HylMailExe}"; Description: "{cm:LaunchProgram,{#HylMailName}}"; \
    Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Only the program directory.  `%APPDATA%\HylMail` and `%LOCALAPPDATA%\HylMail`
; hold the user's mail cache and settings and are never removed silently.
Type: filesandordirs; Name: "{app}"
