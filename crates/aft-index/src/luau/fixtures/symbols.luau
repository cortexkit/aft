-- TODO: exercise parser comments
--[[
TODO: block comment
]]
local marker = "-- TODO: not a comment"
function global() end
local function helper() end
local T = {}
function T.f() helper() end
function T:m() helper() end
T.assigned = function() helper() end
type Private = number
export type Public = { x: number }
local function complex(x)
    if x then helper() end
    if x then helper() end
    if x then helper() end
    if x then helper() end
    if x then helper() end
    if x then helper() end
    if x then helper() end
    if x then helper() end
    if x then helper() end
    if x then helper() end
    if x then helper() end
end
return T
