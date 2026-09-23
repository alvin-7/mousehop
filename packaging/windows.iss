#ifndef StageDir
  #error StageDir is required
#endif
#ifndef OutputDir
  #error OutputDir is required
#endif
#ifndef AppVersion
  #error AppVersion is required
#endif

[Setup]
; Match the upstream Mousehop application identifier.
AppId=com.mousehop.Mousehop
AppName=Mousehop
AppVersion={#AppVersion}
DefaultDirName={localappdata}\Programs\Mousehop
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename=Mousehop-{#AppVersion}-x64-Setup
SetupIconFile=..\mousehop\assets\mousehop.ico
UninstallDisplayIcon={app}\bin\mousehop.exe
LicenseFile={#StageDir}\LICENSE
Compression=lzma2
SolidCompression=yes
DisableProgramGroupPage=yes
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: recursesubdirs createallsubdirs ignoreversion

[Icons]
Name: "{userprograms}\Mousehop"; Filename: "{app}\bin\mousehop.exe"; WorkingDir: "{app}\bin"; IconFilename: "{app}\bin\mousehop.exe"
Name: "{userdesktop}\Mousehop"; Filename: "{app}\bin\mousehop.exe"; WorkingDir: "{app}\bin"; IconFilename: "{app}\bin\mousehop.exe"; Tasks: desktopicon
Name: "{userprograms}\Uninstall Mousehop"; Filename: "{uninstallexe}"; IconFilename: "{app}\bin\mousehop.exe"
