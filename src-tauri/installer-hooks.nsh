; Remove only aliases created by this application, never another app's registration.
!macro CleanupOwnedLinkAliases
  StrCpy $1 0
  ${Do}
    EnumRegValue $0 HKCU "Software\RegisteredApplications" $1
    ${If} $0 == ""
      ${ExitDo}
    ${EndIf}
    ReadRegStr $2 HKCU "Software\RegisteredApplications" "$0"
    ${If} $2 == "Software\Vela\LinkHandlers\1\Capabilities"
    ${OrIf} $2 == "Software\Vela\LinkHandlers\2\Capabilities"
      DeleteRegValue HKCU "Software\RegisteredApplications" "$0"
    ${Else}
      IntOp $1 $1 + 1
    ${EndIf}
  ${Loop}
  DeleteRegKey HKCU "Software\Classes\Vela.CCSwitch.1"
  DeleteRegKey HKCU "Software\Classes\Vela.CCSwitch.2"
  DeleteRegKey HKCU "Software\Vela\LinkHandlers\1"
  DeleteRegKey HKCU "Software\Vela\LinkHandlers\2"
!macroend

!macro NSIS_HOOK_PREINSTALL
!macroend

!macro NSIS_HOOK_POSTINSTALL
  !insertmacro CleanupOwnedLinkAliases
  ReadRegStr $0 HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Vela"
  ${If} $0 != ""
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Vela" '$\"$INSTDIR\vela.exe$\" --vela-login-startup'
  ${EndIf}
  WriteRegStr HKCU "Software\Classes\Vela.CCSwitch.0" "" "Vela CC Switch Link"
  WriteRegStr HKCU "Software\Classes\Vela.CCSwitch.0" "URL Protocol" ""
  WriteRegStr HKCU "Software\Classes\Vela.CCSwitch.0\shell\open\command" "" '$\"$INSTDIR\vela.exe$\" $\"%1$\"'
  WriteRegStr HKCU "Software\Vela\LinkHandlers\0\Capabilities" "ApplicationName" "Vela"
  WriteRegStr HKCU "Software\Vela\LinkHandlers\0\Capabilities" "ApplicationDescription" "CC Switch links"
  WriteRegStr HKCU "Software\Vela\LinkHandlers\0\Capabilities\URLAssociations" "ccswitch" "Vela.CCSwitch.0"
  WriteRegStr HKCU "Software\RegisteredApplications" "Vela Link Vela" "Software\Vela\LinkHandlers\0\Capabilities"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Vela"
  DeleteRegKey HKCU "Software\Classes\Vela.CCSwitch.0"
  !insertmacro CleanupOwnedLinkAliases
  DeleteRegKey HKCU "Software\Vela\LinkHandlers\0"
  DeleteRegKey /ifempty HKCU "Software\Vela\LinkHandlers"
  DeleteRegValue HKCU "Software\RegisteredApplications" "Vela Link Vela"
  ReadRegStr $0 HKCU "Software\Classes\ccswitch\shell\open\command" ""
  ${If} $0 == '$\"$INSTDIR\vela.exe$\" $\"%1$\"'
    DeleteRegKey HKCU "Software\Classes\ccswitch"
  ${EndIf}
!macroend
