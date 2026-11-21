Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")

Dim ROOT
ROOT = fso.GetParentFolderName(WScript.ScriptFullName)
sh.CurrentDirectory = ROOT

' 优先使用 Rust 守护进程；二进制不存在时自动回退旧的 keep_alive.js（可随时回滚）
Dim SUP
SUP = ROOT & "\rust\target\release\dafy-supervisor.exe"
If Not fso.FileExists(SUP) Then SUP = ROOT & "\rust\target\debug\dafy-supervisor.exe"
If fso.FileExists(SUP) Then
  sh.Run """" & SUP & """", 0, False
  WScript.Quit 0
End If

Dim NODE
If fso.FileExists("C:\Program Files\nodejs\node.exe") Then
  NODE = "C:\Program Files\nodejs\node.exe"
ElseIf fso.FileExists("D:\Program Files\nodejs\node.exe") Then
  NODE = "D:\Program Files\nodejs\node.exe"
ElseIf fso.FileExists(sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Programs\nodejs\node.exe")) Then
  NODE = sh.ExpandEnvironmentStrings("%LOCALAPPDATA%\Programs\nodejs\node.exe")
Else
  NODE = "node"
End If

sh.Run """" & NODE & """ """ & ROOT & "\keep_alive.js""", 0, False

sh.CurrentDirectory = ROOT & "\video-server"
sh.Run """" & NODE & """ """ & ROOT & "\video-server\server.js""", 0, False
