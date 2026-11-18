$c = New-Object System.Net.Sockets.TcpClient
$c.ReceiveTimeout = 3000
$c.Connect('127.0.0.1', 45678)
$s = $c.GetStream()
$w = New-Object System.IO.StreamWriter($s)
$w.WriteLine('{"op":"ping"}')
$w.Flush()
$r = New-Object System.IO.StreamReader($s)
try { $r.ReadLine() } catch { 'TIMEOUT: ' + $_.Exception.Message }
$c.Close()
