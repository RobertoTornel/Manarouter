@echo off
setlocal enabledelayedexpansion
set KEY=mr-4798c9325ec415afbf0f11d925312b2480dfdf2f7b958acc62
set BASE=https://pc.nutria-ordinal.ts.net:20900

echo === 1) Relay health over tailnet (port 20900) ===
curl -sS -m 15 %BASE%/healthz
echo.
echo.
echo === 2) Chat completion with hardcoded key ===
curl -sS -m 30 %BASE%/v1/chat/completions ^
  -H "Authorization: Bearer %KEY%" ^
  -H "Content-Type: application/json" ^
  -d "{\"model\":\"expensivesmart\",\"messages\":[{\"role\":\"user\",\"content\":\"Say OK\"}]}" ^
  -w "\nHTTP_STATUS:%%{http_code}\n"
echo.
echo === 3) Model list ===
curl -sS -m 15 %BASE%/v1/models -H "Authorization: Bearer %KEY%" -w "\nHTTP_STATUS:%%{http_code}\n"
echo.
echo If any step times out, run: tailscale ping pc
echo If you see HTTP 401, the key is not recognized on the relay.
pause
