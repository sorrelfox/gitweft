; Gitweft's Windows installer, for Inno Setup 6.3 or later. scripts/dist/windows.sh compiles it
; with what it built as defines: AppVersion (Cargo.toml's), Stage (the folder holding
; gitweft.exe, gitweft.com and the license files), SetupIcon, OutputDir and OutputName.
;
; It installs for the current user by default, into %LOCALAPPDATA%\Programs\Gitweft, without
; administrator rights. The first page offers an installation for all users instead, and
; /CURRENTUSER or /ALLUSERS on the command line choose without asking.

#ifndef AppVersion
  #error Compile it with scripts/dist/windows.sh, which defines AppVersion and the paths
#endif
; Windows' version numbers are digits only: 1.1.0-rc1 is 1.1.0 there.
#define VersionNumbers Copy(AppVersion, 1, Pos("-", AppVersion + "-") - 1)

[Setup]
; The installation's key in the list of installed apps: it must never change.
AppId=io.github.sorrelfox.Gitweft
AppName=Gitweft
AppVersion={#AppVersion}
AppPublisher=sorrelfox
AppPublisherURL=https://github.com/sorrelfox/gitweft
AppSupportURL=https://github.com/sorrelfox/gitweft/issues
VersionInfoVersion={#VersionNumbers}
DefaultDirName={autopf}\Gitweft
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog commandline
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
SetupIconFile={#SetupIcon}
UninstallDisplayIcon={app}\gitweft.exe
UninstallDisplayName=Gitweft
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
OutputDir={#OutputDir}
OutputBaseFilename={#OutputName}

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#Stage}\gitweft.exe"; DestDir: "{app}"; Flags: ignoreversion
; The console twin, for `gitweft` typed in cmd or PowerShell. The folder is not added to PATH.
Source: "{#Stage}\gitweft.com"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\*.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Gitweft"; Filename: "{app}\gitweft.exe"
Name: "{autodesktop}\Gitweft"; Filename: "{app}\gitweft.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\gitweft.exe"; Description: "{cm:LaunchProgram,Gitweft}"; Flags: nowait postinstall skipifsilent
