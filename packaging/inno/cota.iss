; Inno Setup script for Cota.
;   iscc packaging\inno\cota.iss
; Expects target\release\cota.exe to already be built.

#define AppName    "Cota"
; The release workflow passes /DAppVersion=x.y.z; this is the local default.
#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#define AppPublisher "kidchenko"
#define AppURL     "https://github.com/kidchenko/cota"
#define AppExe     "cota.exe"
; Must match notify.rs::APP_ID. Windows resolves a toast's name and icon by
; looking this up against registered shortcuts, so the [Icons] entry below is
; what turns "Windows PowerShell" into "Cota" on every notification the app
; raises. An id with no shortcut behind it shows no toast at all.
#define AppUserModelID "kidchenko.Cota"

[Setup]
; Never reuse this GUID for another product; it is how Windows identifies
; the app across upgrades and uninstalls.
AppId={{3C7E9B14-5D82-4A6F-9E03-8B1D4F2A6C57}
AppName={#AppName}
AppVersion={#AppVersion}
; Without this Add/Remove Programs reads "Cota version 0.1.0".
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}/issues
AppUpdatesURL={#AppURL}/releases
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
LicenseFile=..\..\LICENSE
OutputDir=..\..\dist
OutputBaseFilename=Cota-Setup-{#AppVersion}
SetupIconFile=..\..\assets\icon.ico
UninstallDisplayIcon={app}\{#AppExe}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; The app never needs elevation itself; only writing to Program Files does.
PrivilegesRequired=admin
; Restart Manager closes a running instance during upgrade or uninstall
; instead of leaving a locked file behind.
CloseApplications=yes
CloseApplicationsFilter=*.exe
; The [Code] section intentionally touches HKCU; see the note there.
UsedUserAreasWarning=no
; Toast notifications need the WinRT APIs, and the panel asks DWM for rounded
; corners (harmlessly ignored on 10).
MinVersion=10.0

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "..\..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; AppUserModelID is the entire reason this shortcut is not optional. See the
; note on the #define above.
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"; \
  AppUserModelID: "{#AppUserModelID}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"

[Run]
; --show-panel so a first run shows the numbers rather than adding a silent
; icon to a tray that already has eleven of them. Run-at-login is a toggle in
; the app's menu: it writes HKCU itself, which keeps it attached to the right
; user even though this installer runs elevated.
Filename: "{app}\{#AppExe}"; Parameters: "--show-panel"; \
  Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Stop the tray instance before files are removed.
Filename: "{sys}\taskkill.exe"; Parameters: "/f /im {#AppExe}"; \
  Flags: runhidden; RunOnceId: "KillCota"

[UninstallDelete]
; Runtime debris, not settings. The log is a support artefact and state.json is
; a few hundred samples of a percentage; neither is worth leaving on someone
; else's disk. config.json is deliberately absent from this list -- see below.
Type: files; Name: "{userappdata}\{#AppName}\cota.log"
Type: files; Name: "{userappdata}\{#AppName}\cota.log.1"
Type: files; Name: "{userappdata}\{#AppName}\state.json"

[Code]
// The autostart entry is per-user and written by the app, so the uninstaller
// has to clear it explicitly or Windows keeps trying to launch a binary that
// is no longer there.
//
// Caveat, and the reason for UsedUserAreasWarning=no above: under UAC an
// admin elevating their own account keeps the same HKCU, so this works in the
// normal case. If a *different* admin supplies credentials, this clears their
// Run key rather than the installing user's, and the original user is left to
// remove a dead startup entry by hand. Writing to HKLM instead would be worse
// -- it would force autostart on every account on the machine.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    RegDeleteValue(HKEY_CURRENT_USER,
      'Software\Microsoft\Windows\CurrentVersion\Run', 'Cota');

    // Only succeeds if config.json is gone, which is the point: thresholds and
    // the poll interval survive a reinstall, and a user who never changed them
    // gets the directory cleaned up.
    RemoveDir(ExpandConstant('{userappdata}\Cota'));
    RemoveDir(ExpandConstant('{app}'));
  end;
end;
