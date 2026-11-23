; Tapo Viewer's installer, built with NSIS (tools/build-installer.sh).
;
; Installs for the current user, with no administrator rights, into
; %LOCALAPPDATA%\Programs\TapoViewer, with a Start menu shortcut, an optional
; desktop shortcut and an entry in Settings > Apps for uninstalling.
;
; The app's updater runs it silently: TapoViewer-Setup.exe /S /RELAUNCH
; installs over the running version once it has closed, then starts the app.
; Settings and recordings (in %APPDATA%\TapoViewer and Videos) are kept on
; update and on uninstall.

Unicode true
; 64-bit, like the app (Windows 10 and 11 x64).
Target amd64-unicode
SetCompressor /SOLID lzma

!ifndef VERSION
  !error "Pass the version: makensis -DVERSION=1.2.3"
!endif
!ifndef APP_EXE
  !define APP_EXE "TapoViewer.exe"
!endif
!ifndef OUT_DIR
  !define OUT_DIR "."
!endif

!define APP_NAME "Tapo Viewer"
!define APP_ID "TapoViewer"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_ID}"

Name "${APP_NAME}"
OutFile "${OUT_DIR}\TapoViewer-Setup-${VERSION}.exe"
InstallDir "$LOCALAPPDATA\Programs\${APP_ID}"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
RequestExecutionLevel user
BrandingText "${APP_NAME} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP_NAME}"
VIAddVersionKey "FileDescription" "${APP_NAME} installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "MIT licence"

!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"

!define MUI_ICON "..\assets\app.ico"
!define MUI_UNICON "..\assets\app.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "Start ${APP_NAME}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; The app may still be closing (after starting an update) or be open (when
; installing by hand): wait for its file to be free.
!macro WaitForApp
  StrCpy $0 0
  ${Do}
    ClearErrors
    ${If} ${FileExists} "$INSTDIR\${APP_EXE}"
      Rename "$INSTDIR\${APP_EXE}" "$INSTDIR\${APP_EXE}.old"
    ${EndIf}
    ${IfNot} ${Errors}
      ${Break}
    ${EndIf}
    IntOp $0 $0 + 1
    ${If} $0 > 40
      ${If} ${Silent}
        Abort
      ${EndIf}
      MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "${APP_NAME} is still open. Close it, then press Retry." IDRETRY +2
      Abort
      StrCpy $0 0
    ${EndIf}
    Sleep 250
  ${Loop}
  Delete "$INSTDIR\${APP_EXE}.old"
!macroend

Section "${APP_NAME}" SecApp
  SectionIn RO
  SetOutPath "$INSTDIR"
  !insertmacro WaitForApp
  File "/oname=${APP_EXE}" "${OUT_DIR}\${APP_EXE}"
  File "/oname=README.md" "..\README.md"
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  CreateShortCut "$SMPROGRAMS\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"

  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "${APP_NAME}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "EstimatedSize" $0
SectionEnd

Section "Desktop shortcut" SecDesktop
  CreateShortCut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecApp} "The app and its Start menu shortcut."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "A shortcut on the desktop."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Function .onInit
  ; An update keeps the shortcuts the user has: a silent install adds a
  ; desktop shortcut only if there is one already.
  ${If} ${Silent}
  ${AndIfNot} ${FileExists} "$DESKTOP\${APP_NAME}.lnk"
    SectionSetFlags ${SecDesktop} 0
  ${EndIf}
FunctionEnd

Function .onInstSuccess
  ; After an update from the app: start it again.
  ${GetParameters} $0
  ClearErrors
  ${GetOptions} $0 "/RELAUNCH" $1
  ${IfNot} ${Errors}
    Exec '"$INSTDIR\${APP_EXE}"'
  ${EndIf}
FunctionEnd

Section "Uninstall"
  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${APP_NAME}.lnk"
  Delete "$DESKTOP\${APP_NAME}.lnk"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
  ; The unpacked ffmpeg; settings and recordings stay.
  RMDir /r "$LOCALAPPDATA\${APP_ID}\data"
SectionEnd
