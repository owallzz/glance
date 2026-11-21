; glance installer: per-user, no admin rights.
;   iscc /DAppVersion=0.2.0 /DSourceExe=path\to\glance.exe /DOutputName=glance-v0.2.0-windows-x86_64-setup packaging\windows\glance.iss
; Installs to %LOCALAPPDATA%\Programs\glance (the updater relies on that
; path), adds a Start menu shortcut and the glance:// link handler, and
; registers an uninstaller. Upgrades install over the old copy in place.
; Silent upgrade: setup.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART
; (add /LAUNCH=1 to start glance again afterwards).

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\..\target\release\glance.exe"
#endif
#ifndef OutputName
  #define OutputName "glance-setup"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\dist"
#endif

[Setup]
; Never change AppId: upgrades and the uninstaller find the install by it.
AppId={{5B0C8F0E-3C1D-4E7A-9A55-6C2F1D9E4B21}
AppName=glance
AppVersion={#AppVersion}
AppVerName=glance {#AppVersion}
AppPublisher=glance contributors
AppPublisherURL=https://github.com/owallzz/glance
AppSupportURL=https://github.com/owallzz/glance/issues
AppUpdatesURL=https://github.com/owallzz/glance/releases
VersionInfoVersion={#AppVersion}
VersionInfoProductName=glance
VersionInfoDescription=glance installer
VersionInfoCompany=glance contributors
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\glance
DisableDirPage=yes
DefaultGroupName=glance
DisableProgramGroupPage=yes
DisableReadyPage=yes
UsePreviousAppDir=no
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename={#OutputName}
SetupIconFile=glance.ico
UninstallDisplayIcon={app}\glance.exe
UninstallDisplayName=glance
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
; A running glance holds glance.exe open: close it (silent upgrades too).
CloseApplications=force
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "glance.exe"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\glance"; Filename: "{app}\glance.exe"; Comment: "Peer-to-peer screen sharing"
Name: "{userdesktop}\glance"; Filename: "{app}\glance.exe"; Tasks: desktopicon

[Registry]
; glance:// links (the app writes the same key on start; this makes links
; work before the first launch and lets the uninstaller remove it).
Root: HKCU; Subkey: "Software\Classes\glance"; ValueType: string; ValueName: ""; ValueData: "URL:glance"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\glance"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKCU; Subkey: "Software\Classes\glance\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\glance.exe"",0"
Root: HKCU; Subkey: "Software\Classes\glance\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\glance.exe"" ""%1"""

[Run]
Filename: "{app}\glance.exe"; Description: "Launch glance"; Flags: nowait postinstall skipifsilent
; Silent upgrades started with /LAUNCH=1 bring glance back afterwards.
Filename: "{app}\glance.exe"; Flags: nowait; Check: LaunchAfterSilentInstall

[UninstallDelete]
Type: dirifempty; Name: "{app}"

[Code]
function LaunchAfterSilentInstall: Boolean;
begin
  Result := WizardSilent and (ExpandConstant('{param:LAUNCH|0}') = '1');
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    { Start on login, written by the app itself. }
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'glance');
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run', 'glance');
  end;
  if CurUninstallStep = usPostUninstall then
  begin
    { Identity, contacts and settings stay unless the user asks. }
    if (not UninstallSilent) and DirExists(ExpandConstant('{userappdata}\glance')) then
      if MsgBox('Also delete your glance data (identity, contacts, settings and received-file cache)?',
                mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
        DelTree(ExpandConstant('{userappdata}\glance'), True, True, True);
  end;
end;
