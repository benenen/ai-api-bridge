-- Availability probe for OpenCode Zen / the "go" endpoint.
--
-- Zen exposes no quota endpoint, so this just checks reachability via /models
-- (an authenticated 200 = available). No quota numbers are reported.
--
-- Context passed in: ctx = { name, base_url, api_key, extra_headers, wire }
-- Return: { ok = bool, remaining?, used?, limit?, note? }

local resp = http {
    url = ctx.base_url .. "/models",
    headers = { authorization = "Bearer " .. (ctx.api_key or "") },
}

return {
    ok = resp.status == 200,
    note = "GET /models -> " .. resp.status,
}
