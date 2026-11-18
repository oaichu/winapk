param([string]$Send, [int]$ReadMs = 4000)
$c = New-Object System.Net.Sockets.TcpClient
$c.ReceiveTimeout = $ReadMs
$c.Connect('127.0.0.1', 45678)
$s = $c.GetStream()
if ($Send) {
    $w = New-Object System.IO.StreamWriter($s)
    $w.Write($Send)
    $w.Flush()
    Start-Sleep -Milliseconds 300
}
$r = New-Object System.IO.StreamReader($s)
try { $r.ReadLine() } catch { 'TIMEOUT' }
$c.Close()
