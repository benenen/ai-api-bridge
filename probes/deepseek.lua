-- Availability + usage probe for DeepSeek official API.
--
-- First tries GET /user/balance (DeepSeek billing endpoint, at the root level
-- outside /v1). Falls back to GET /models for a connectivity check.
--
-- Context passed in: ctx = { name, base_url, api_key, extra_headers, wire }
-- Return: { ok = bool, remaining?, used?, limit?, note? }

local headers = { authorization = "Bearer " .. (ctx.api_key or "") }

-- DeepSeek /user/balance is at the API root, not under /v1.
-- Strip trailing "/v1" from base_url to get the root.
local root_url = string.gsub(ctx.base_url, "/v1$", "")

-- Try /user/balance first.
local resp = http {
    url = root_url .. "/user/balance",
    headers = headers,
}

if resp.status == 200 then
    local body = json_decode(resp.body)
    local available = body.is_available or false
    local note_parts = {}
    local total_balance = 0

    if body.balance_infos then
        for _, info in ipairs(body.balance_infos) do
            local currency = info.currency or "?"
            local total = tonumber(info.total_balance) or 0
            total_balance = total_balance + total
            table.insert(note_parts, currency .. ": total=" .. tostring(info.total_balance)
                .. " topped_up=" .. tostring(info.topped_up_balance or "0")
                .. " granted=" .. tostring(info.granted_balance or "0"))
        end
    end

    local note = "available=" .. tostring(available)
    if #note_parts > 0 then
        note = note .. " | " .. table.concat(note_parts, "; ")
    end

    return {
        ok = true,
        remaining = total_balance,
        used = 0,
        limit = total_balance,
        note = note,
    }
end

-- /user/balance not available — fall back to /models ping.
local ping = http {
    url = ctx.base_url .. "/models",
    headers = headers,
}
return {
    ok = ping.status == 200,
    note = "GET /user/balance -> " .. resp.status .. " (fallback /models -> " .. ping.status .. ")",
}
