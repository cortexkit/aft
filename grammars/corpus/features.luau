--!strict
-- TODO: single-line comment
--[=[
TODO: block comment with a scanner delimiter
]=]
export type Box<T> = { value: T, label: string? }
type Callback = (number, ...string) -> (number, string)

local text = [==[a long string with ]=] inside]==]
local escaped = "quoted \"value\" and whitespace\z  continued"
local function choose<T>(value: T, fallback: T): T
    return if value ~= nil then value else fallback
end

local M = { nested = { value = 1 } }
function M.add(a: number, b: number): number
    return a + b
end
function M:method(count: number)
    local total = 0
    for i = 1, count do
        if i % 2 == 0 then
            continue
        elseif i > 20 then
            break
        else
            total += i
        end
    end
    while total < count do total += 1 end
    repeat total -= 1 until total <= count
    for key, value in pairs(self.nested) do
        print(key, value)
    end
    return total :: number
end
M.callback = function(...) return ... end
local sibling = require("./sibling")
local alias = require("@shared/utils")
local instance = require(script.Parent:WaitForChild("Sibling"))
local index = require(script.Parent["Sibling"])
return M
