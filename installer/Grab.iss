; Grab Windows installer (Inno Setup 6)
; Build from repository root after `cargo build --release`.

#ifndef AppVersion
  #define AppVersion "1.1.3"
#endif

#define AppName "Grab"
#define AppExe "grab.exe"

[Setup]
AppId={{A2C7C89D-6D31-4B1E-9B0B-2B90B3D8A61F}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Grab
AppPublisherURL=https://github.com/
DefaultDirName={localappdata}\Programs\Grab
DefaultGroupName=Grab
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=Grab-Setup-x64
SetupIconFile=..\assets\grab.ico
UninstallDisplayIcon={app}\{#AppExe}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64
; Keep the installer per-user so app updates do not need administrator rights.
CloseApplications=yes
RestartApplications=no
Uninstallable=yes

[Languages]
Name: "ukrainian"; MessagesFile: "compiler:Languages\Ukrainian.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Створити ярлик на робочому столі"; GroupDescription: "Додаткові параметри:"; Flags: unchecked

[Files]
Source: "..\target\release\grab.exe"; DestDir: "{app}"; DestName: "grab.exe"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Grab"; Filename: "{app}\grab.exe"
Name: "{autodesktop}\Grab"; Filename: "{app}\grab.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\grab.exe"; Description: "Запустити Grab"; Flags: postinstall nowait skipifsilent
