!macro NSIS_HOOK_POSTINSTALL
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
  DeleteRegKey HKCU "Software\Classes\gptSwitch.CCSwitch.0"
  DeleteRegKey HKCU "Software\gpt-Switch\LinkHandlers\0"
  DeleteRegKey /ifempty HKCU "Software\gpt-Switch\LinkHandlers"
  DeleteRegValue HKCU "Software\RegisteredApplications" "Vela Link Vela"
  ReadRegStr $0 HKCU "Software\Classes\ccswitch\shell\open\command" ""
  ${If} $0 == '$\"$INSTDIR\vela.exe$\" $\"%1$\"'
    DeleteRegKey HKCU "Software\Classes\ccswitch"
  ${EndIf}
!macroend
