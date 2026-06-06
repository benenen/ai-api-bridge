-- TEMPLATE: probe a vendor that exposes a JSON credits/usage endpoint.
--
-- Copy this and adapt the URL, headers, and field names to your provider — the
-- bridge never hardcodes any vendor's API, the script does it all.
--
-- Helpers available: http{ url, method="GET", headers={}, body=nil } -> { status, body }
--                    json_decode(str) -> table,  json_encode(table) -> str
-- Context:           ctx = { name, base_url, api_key, extra_headers, wire }
-- Return:            { ok = bool, remaining?, used?, limit?, note? }

local resp = http {
    url = ctx.base_url .. "/credits",
    headers = { authorization = "Bearer " .. (ctx.api_key or "") },
}

if resp.status ~= 200 then
    return { ok = false, note = "GET /credits -> " .. resp.status }
end

local body = json_decode(resp.body)
return {
    ok = true,
    remaining = body.remaining, -- credits left (used for `quota_min` failover)
    used = body.used,
    limit = body.limit,
    note = "credits ok",
}
