; Windows installer (Inno Setup 6). Built in CI after PyInstaller:
;   iscc /DAppVersion=1.0.0 packaging\windows\orchard-commander.iss
; Output: dist\Orchard-Commander-Setup.exe
; Installs for the current user or for all users (the user chooses), adds a
; Start menu entry, an optional desktop shortcut and an uninstaller. Silent
; installs (winget, Chocolatey): /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{46BEE22F-5C1E-5898-AFC7-E42B017F6AAB}
AppName=Orchard Commander
AppVersion={#AppVersion}
AppVerName=Orchard Commander {#AppVersion}
AppPublisher=Stephan Lengl
AppPublisherURL=https://sandboxly.github.io/orchard-commander/
AppSupportURL=https://github.com/sandboxly/orchard-commander/issues
AppUpdatesURL=https://github.com/sandboxly/orchard-commander/releases
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\Orchard Commander
DefaultGroupName=Orchard Commander
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=commandline dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
LicenseFile=..\..\LICENSE
SetupIconFile=..\icons\app.ico
UninstallDisplayIcon={app}\orchard-commander.exe
UninstallDisplayName=Orchard Commander
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
OutputDir=..\..\dist
OutputBaseFilename=Orchard-Commander-Setup
CloseApplications=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\..\dist\orchard-commander\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[InstallDelete]
; A new version replaces the old bundle completely (no stale libraries left behind).
Type: filesandordirs; Name: "{app}\_internal"

[Icons]
Name: "{autoprograms}\Orchard Commander"; Filename: "{app}\orchard-commander.exe"
Name: "{autodesktop}\Orchard Commander"; Filename: "{app}\orchard-commander.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\orchard-commander.exe"; Description: "{cm:LaunchProgram,Orchard Commander}"; Flags: nowait postinstall skipifsilent
