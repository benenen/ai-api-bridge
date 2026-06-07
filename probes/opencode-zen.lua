-- Availability + usage probe for OpenCode Zen / the "go" endpoint.
--
-- First tries GET /usage (rolling5h/weekly/monthly dollar usage). If the
-- endpoint doesn't exist yet (404), falls back to GET /models for a
-- connectivity check. When /usage goes live, the probe starts reporting
-- real quota numbers automatically.
--
-- Context passed in: ctx = { name, base_url, api_key, extra_headers, wire }
-- Return: { ok = bool, remaining?, used?, limit?, note? }

local headers = { authorization = "Bearer " .. (ctx.api_key or "") }

-- Try /usage first.
local resp = http {
    url = ctx.base_url .. "/usage",
    headers = headers,
}

if resp.status == 200 then
    local body = json_decode(resp.body)
    local h5 = body.rolling5h or {}
    local wk = body.weekly or {}
    local mo = body.monthly or {}
    local used = h5.usageDollars or 0
    local limit = h5.limitDollars or 0
    local remaining = limit - used
    local note = "5h: $" .. tostring(used) .. "/$" .. tostring(limit)
        .. " (" .. tostring(h5.usagePercent or 0) .. "%)"
        .. " | week: $" .. tostring(wk.usageDollars or 0)
        .. "/$" .. tostring(wk.limitDollars or 0)
        .. " | month: $" .. tostring(mo.usageDollars or 0)
        .. "/$" .. tostring(mo.limitDollars or 0)
    return { ok = true, remaining = remaining, used = used, limit = limit, note = note }
end

-- /usage not available (404 or other non-200) — fall back to /models ping.
local ping = http {
    url = ctx.base_url .. "/models",
    headers = headers,
}
return {
    ok = ping.status == 200,
    note = "GET /usage -> " .. resp.status .. " (fallback /models -> " .. ping.status .. ")",
}
