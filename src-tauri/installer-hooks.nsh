; Remove only aliases created by this application, never another app's registration.
!macro CleanupOwnedLinkAliases
  StrCpy $1 0
  ${Do}
    EnumRegValue $0 HKCU "Software\RegisteredApplications" $1
    ${If} $0 == ""
      ${ExitDo}
    ${EndIf}
    ReadRegStr $2 HKCU "Software\RegisteredApplications" "$0"
    ${If} $2 == "Software\gpt-Switch\LinkHandlers\1\Capabilities"
    ${OrIf} $2 == "Software\gpt-Switch\LinkHandlers\2\Capabilities"
      DeleteRegValue HKCU "Software\RegisteredApplications" "$0"
    ${Else}
      IntOp $1 $1 + 1
    ${EndIf}
  ${Loop}
  DeleteRegKey HKCU "Software\Classes\gptSwitch.CCSwitch.1"
  DeleteRegKey HKCU "Software\Classes\gptSwitch.CCSwitch.2"
  DeleteRegKey HKCU "Software\gpt-Switch\LinkHandlers\1"
  DeleteRegKey HKCU "Software\gpt-Switch\LinkHandlers\2"
  DeleteRegValue HKCU "Software\RegisteredApplications" "gpt-Switch Link gpt-Switch"
  DeleteRegValue HKCU "Software\RegisteredApplications" "lich13-switch Link lich13-switch"
!macroend

!macro NSIS_HOOK_PREINSTALL
  ReadRegStr $0 SHCTX "${UNINSTKEY}" "MainBinaryName"
  ${If} $0 == ""
    ReadRegStr $0 SHCTX "Software\Microsoft\Windows\CurrentVersion\Uninstall\gpt-Switch" "MainBinaryName"
  ${EndIf}
  ${If} $0 == "gpt-switch.exe"
    !insertmacro CheckIfAppIsRunning "$INSTDIR\gpt-switch.exe" "gpt-Switch"
  ${ElseIf} $0 == "lich13-switch.exe"
    !insertmacro CheckIfAppIsRunning "$INSTDIR\lich13-switch.exe" "lich13-switch"
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTINSTALL
  !insertmacro CleanupOwnedLinkAliases
  !insertmacro IsShortcutTarget "$SMPROGRAMS\gpt-Switch.lnk" "$INSTDIR\gpt-switch.exe"
  Pop $0
  ${If} $0 = 1
    Delete "$SMPROGRAMS\gpt-Switch.lnk"
  ${EndIf}
  !insertmacro IsShortcutTarget "$DESKTOP\gpt-Switch.lnk" "$INSTDIR\gpt-switch.exe"
  Pop $0
  ${If} $0 = 1
    Delete "$DESKTOP\gpt-Switch.lnk"
  ${EndIf}
  !insertmacro IsShortcutTarget "$SMPROGRAMS\lich13-switch.lnk" "$INSTDIR\lich13-switch.exe"
  Pop $0
  ${If} $0 = 1
    Delete "$SMPROGRAMS\lich13-switch.lnk"
  ${EndIf}
  !insertmacro IsShortcutTarget "$DESKTOP\lich13-switch.lnk" "$INSTDIR\lich13-switch.exe"
  Pop $0
  ${If} $0 = 1
    Delete "$DESKTOP\lich13-switch.lnk"
  ${EndIf}
  ReadRegStr $0 HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "gpt-Switch"
  ${If} $0 != ""
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Vela" '$\"$INSTDIR\vela.exe$\" --vela-login-startup'
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "gpt-Switch"
  ${EndIf}
  ReadRegStr $0 HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Vela"
  ${If} $0 != ""
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Vela" '$\"$INSTDIR\vela.exe$\" --vela-login-startup'
  ${EndIf}
  WriteRegStr HKCU "Software\Classes\gptSwitch.CCSwitch.0" "" "Vela CC Switch Link"
  WriteRegStr HKCU "Software\Classes\gptSwitch.CCSwitch.0" "URL Protocol" ""
  WriteRegStr HKCU "Software\Classes\gptSwitch.CCSwitch.0\shell\open\command" "" '$\"$INSTDIR\vela.exe$\" $\"%1$\"'
  WriteRegStr HKCU "Software\gpt-Switch\LinkHandlers\0\Capabilities" "ApplicationName" "Vela"
  WriteRegStr HKCU "Software\gpt-Switch\LinkHandlers\0\Capabilities" "ApplicationDescription" "CC Switch links"
  WriteRegStr HKCU "Software\gpt-Switch\LinkHandlers\0\Capabilities\URLAssociations" "ccswitch" "gptSwitch.CCSwitch.0"
  WriteRegStr HKCU "Software\RegisteredApplications" "Vela Link Vela" "Software\gpt-Switch\LinkHandlers\0\Capabilities"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Vela"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "gpt-Switch"
  DeleteRegKey HKCU "Software\Classes\gptSwitch.CCSwitch.0"
  !insertmacro CleanupOwnedLinkAliases
  DeleteRegKey HKCU "Software\gpt-Switch\LinkHandlers\0"
  DeleteRegKey /ifempty HKCU "Software\gpt-Switch\LinkHandlers"
  DeleteRegValue HKCU "Software\RegisteredApplications" "Vela Link Vela"
  ReadRegStr $0 HKCU "Software\Classes\ccswitch\shell\open\command" ""
  ${If} $0 == '$\"$INSTDIR\vela.exe$\" $\"%1$\"'
  ${OrIf} $0 == '$\"$INSTDIR\lich13-switch.exe$\" $\"%1$\"'
  ${OrIf} $0 == '$\"$INSTDIR\gpt-switch.exe$\" $\"%1$\"'
    DeleteRegKey HKCU "Software\Classes\ccswitch"
  ${EndIf}
!macroend
