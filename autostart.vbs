Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")

Dim ROOT
ROOT = fso.GetParentFolderName(WScript.ScriptFullName)
sh.CurrentDirectory = ROOT

' 启动 Rust 守护进程（优先 release，其次 debug）；它负责拉起 bridge / TTS / ofm / pc / video
Dim SUP
SUP = ROOT & "\rust\target\release\dafy-supervisor.exe"
If Not fso.FileExists(SUP) Then SUP = ROOT & "\rust\target\debug\dafy-supervisor.exe"
If fso.FileExists(SUP) Then
  sh.Run """" & SUP & """", 0, False
  WScript.Quit 0
End If

' 未找到守护进程二进制：提示用户
MsgBox "未找到大肥鱼守护进程（rust\target\release\dafy-supervisor.exe）。" & vbCrLf & _
       "请先在 rust 目录执行 cargo build --release，或重新运行安装包。", vbExclamation, "大肥鱼"
