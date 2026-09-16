# A throwaway listener that records what a harness actually sends.
#
# Used to settle a question that reading code and reading logs could not: when a Hermes
# session in an isolated profile fails with a 401 whose wording belongs to a provider it
# was not configured to use, where is the request actually going?
#
# It answers with a valid OpenAI-shaped response, so if the harness reaches it the run
# succeeds and the capture proves the routing. If nothing arrives, the request went
# somewhere else and the capture file is empty -- which is itself the answer.

$ErrorActionPreference = 'Stop'
$port = if ($args.Count -ge 1) { $args[0] } else { '8772' }
$out = Join-Path $env:TEMP "hermes-capture-$port.txt"
Remove-Item $out -Force -ErrorAction SilentlyContinue

$listener = [System.Net.HttpListener]::new()
$listener.Prefixes.Add("http://127.0.0.1:$port/")
$listener.Start()

# One request is enough for the question; the caller stops the job afterwards.
$context = $listener.GetContext()
$request = $context.Request

"$($request.HttpMethod) $($request.RawUrl)" | Add-Content $out
foreach ($key in $request.Headers.AllKeys) {
    $value = $request.Headers[$key]
    # Never record a credential, even a throwaway one: a capture file that leaks a key
    # into a temp directory is a worse outcome than the diagnosis is worth.
    if ($key -match '^(?i)authorization|x-api-key') { $value = "<redacted len=$($value.Length)>" }
    "  ${key}: $value" | Add-Content $out
}

$body = @'
{"id":"capture","object":"chat.completion","created":0,"model":"capture",
 "choices":[{"index":0,"message":{"role":"assistant","content":"CAPTURED"},"finish_reason":"stop"}],
 "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}
'@
$bytes = [System.Text.Encoding]::UTF8.GetBytes($body)
$context.Response.StatusCode = 200
$context.Response.ContentType = 'application/json'
$context.Response.OutputStream.Write($bytes, 0, $bytes.Length)
$context.Response.Close()
$listener.Stop()
