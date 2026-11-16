local A = require('./b')
local B = require('@Root/b')
local C = require(script.Parent.b)
local D = require(script.Parent["b"])
local E = require(script.Parent:WaitForChild("b"))
local F = require(script.Parent:FindFirstChild("b"))
local G = require('./ambig')
local H = require('./server.server')
local I = require(script.Parent.native)
local J = require(game:GetService("ReplicatedStorage").b)
local K = require(script.Parent[name])

local function string_call() A.target() end
local function alias_call() B.target() end
local function dot_call() C.target() end
local function bracket_call() D.target() end
local function wait_call() E.target() end
local function find_call() F.target() end
local function ambiguous_call() G.target() end
local function server_call() H.target() end
local function native_call() I.target() end
local function service_call() J.target() end
local function computed_call() K.target() end
