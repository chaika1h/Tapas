; Tapas's installer, built with NSIS (tools/build-installer.sh).
;
; Installs for the current user, with no administrator rights, into
; %LOCALAPPDATA%\Programs\Tapas, with a Start menu shortcut, an optional
; desktop shortcut and an entry in Settings > Apps for uninstalling.
;
; The app's updater runs it silently: Tapas-Setup.exe /S /RELAUNCH
; installs over the running version once it has closed, then starts the app.
; Settings and recordings (in %APPDATA%\Tapas and Videos) are kept on
; update and on uninstall. It replaces an installed Tapo Viewer, the app's
; former name.

Unicode true
; 64-bit, like the app (Windows 10 and 11 x64).
Target amd64-unicode
SetCompressor /SOLID lzma

!ifndef VERSION
  !error "Pass the version: makensis -DVERSION=1.2.3"
!endif
!ifndef APP_EXE
  !define APP_EXE "Tapas.exe"
!endif
!ifndef OUT_DIR
  !define OUT_DIR "."
!endif

!define APP_NAME "Tapas"
!define APP_ID "Tapas"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_ID}"

Name "${APP_NAME}"
OutFile "${OUT_DIR}\Tapas-Setup-${VERSION}.exe"
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

; Whether the installed app is running ($R9 = 1): a running program's file
; can't be opened for writing. (Renaming it is no test: Windows lets a running
; program's file be renamed.)
!macro AppRunning
  StrCpy $R9 0
  ${If} ${FileExists} "$INSTDIR\${APP_EXE}"
    ClearErrors
    FileOpen $R8 "$INSTDIR\${APP_EXE}" a
    ${If} ${Errors}
      StrCpy $R9 1
    ${Else}
      FileClose $R8
    ${EndIf}
  ${EndIf}
!macroend

; The app may still be closing (after starting an update) or be open (when
; installing or uninstalling by hand): close it and wait until its file is
; free. `what` finishes the question: "install the new version".
!macro WaitForApp what
  ; Left by earlier installers, which renamed the app's file out of the way.
  Delete "$INSTDIR\${APP_EXE}.old"
  !insertmacro AppRunning
  ${If} $R9 == 1
  ${AndIfNot} ${Silent}
    MessageBox MB_OKCANCEL|MB_ICONINFORMATION "${APP_NAME} is open. Press OK to close it and ${what}." IDOK +2
    Abort
  ${EndIf}
  StrCpy $R7 0
  ${DoWhile} $R9 == 1
    ${If} $R7 == 0
      DetailPrint "Closing ${APP_NAME}…"
      ; Asks it to close, as its window's close button does. (An update
      ; needn't: the app is closing by itself.)
      ${IfNot} ${Silent}
        nsExec::Exec 'taskkill /IM "${APP_EXE}"'
        Pop $R6
      ${EndIf}
    ${EndIf}
    ; Still running after 8 s: end it.
    ${If} $R7 == 32
      nsExec::Exec 'taskkill /F /IM "${APP_EXE}"'
      Pop $R6
    ${EndIf}
    ${If} $R7 > 80
      ${If} ${Silent}
        Abort
      ${EndIf}
      MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "${APP_NAME} couldn't be closed. Close it, then press Retry." IDRETRY +2
      Abort
      StrCpy $R7 1
    ${EndIf}
    IntOp $R7 $R7 + 1
    Sleep 250
    !insertmacro AppRunning
  ${Loop}
!macroend

; The app was called Tapo Viewer: an installed Tapo Viewer is closed (by the
; updater) and removed, keeping its settings, which Tapas takes over.
!define OLD_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\TapoViewer"
Function RemoveTapoViewer
  ReadRegStr $R0 HKCU "${OLD_KEY}" "InstallLocation"
  ${If} $R0 == ""
    Return
  ${EndIf}
  ; Close it if it's open.
  nsExec::Exec 'taskkill /IM "TapoViewer.exe"'
  Pop $R2
  StrCpy $R1 0
  ${Do}
    ClearErrors
    Delete "$R0\TapoViewer.exe"
    ${IfNot} ${FileExists} "$R0\TapoViewer.exe"
      ${Break}
    ${EndIf}
    IntOp $R1 $R1 + 1
    ${If} $R1 == 20
      nsExec::Exec 'taskkill /F /IM "TapoViewer.exe"'
      Pop $R2
    ${EndIf}
    ${If} $R1 > 40
      ${Break}
    ${EndIf}
    Sleep 250
  ${Loop}
  Delete "$R0\README.md"
  Delete "$R0\Uninstall.exe"
  RMDir "$R0"
  Delete "$SMPROGRAMS\Tapo Viewer.lnk"
  ${If} ${FileExists} "$DESKTOP\Tapo Viewer.lnk"
    Delete "$DESKTOP\Tapo Viewer.lnk"
    CreateShortCut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
  ${EndIf}
  RMDir /r "$LOCALAPPDATA\TapoViewer\data"
  DeleteRegKey HKCU "${OLD_KEY}"
FunctionEnd

Section "${APP_NAME}" SecApp
  SectionIn RO
  SetOutPath "$INSTDIR"
  !insertmacro WaitForApp "install the new version"
  Call RemoveTapoViewer
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
  !insertmacro WaitForApp "uninstall it"
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
