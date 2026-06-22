--[[
	Captures the Studio Output (prints, warnings, and errors) for the context this
	session runs in and streams it to the connected Rojo server via
	`ApiContext:feedback`. That server buffers it so external tooling (the
	`rojo logs` CLI and the `read_logs` MCP tool) can read what happened at
	runtime — the plugin half of Rojo's runtime-feedback loop.

	One LogCapture is owned by each ServeSession and started once it reaches
	Status.Connected, so the edit-context session reports "edit" output and a
	playtest-server session (autoConnectPlaytestServer) reports "server"/"client"
	output, each tagged with its run mode.
]]

local RunService = game:GetService("RunService")
local LogService = game:GetService("LogService")

local Settings = require(script.Parent.Settings)

-- Flush pending entries at most this often (seconds), or sooner once a batch
-- reaches MAX_BATCH, so a chatty game makes few requests but stays responsive.
local FLUSH_INTERVAL = 0.25
local MAX_BATCH = 100

local MESSAGE_TYPE_TO_LEVEL = {
	[Enum.MessageType.MessageOutput] = "print",
	[Enum.MessageType.MessageInfo] = "info",
	[Enum.MessageType.MessageWarning] = "warning",
	[Enum.MessageType.MessageError] = "error",
}

-- Rojo's own log lines are tagged (see plugin/log/init.lua). We must skip them,
-- or capturing our own output — including anything logged while POSTing
-- feedback — would feed itself and create a storm.
local function isRojoLog(message)
	return string.find(message, "[Rojo-", 1, true) ~= nil
end

-- The run context this session is observing. Edit-mode sessions report "edit";
-- a session running inside a playtest reports "server"/"client".
local function runMode()
	if RunService:IsRunning() then
		if RunService:IsServer() then
			return "server"
		elseif RunService:IsClient() then
			return "client"
		end
		return "server"
	end
	return "edit"
end

local LogCapture = {}
LogCapture.__index = LogCapture

function LogCapture.new(apiContext)
	return setmetatable({
		__apiContext = apiContext,
		__connection = nil,
		__pending = {},
		__flushScheduled = false,
	}, LogCapture)
end

-- Begins capturing. Idempotent, and a no-op if the user disabled the
-- `captureOutput` setting. Called when the session reaches Status.Connected.
function LogCapture:start()
	if self.__connection then
		return
	end
	if not Settings:get("captureOutput") then
		return
	end

	self.__connection = LogService.MessageOut:Connect(function(message, messageType)
		self:__capture(message, messageType)
	end)
end

function LogCapture:__capture(message, messageType)
	if isRojoLog(message) then
		return
	end

	table.insert(self.__pending, {
		timestampUnixMs = DateTime.now().UnixTimestampMillis,
		level = MESSAGE_TYPE_TO_LEVEL[messageType] or "print",
		message = message,
		runMode = runMode(),
	})

	if #self.__pending >= MAX_BATCH then
		self:__flush()
	else
		self:__scheduleFlush()
	end
end

function LogCapture:__scheduleFlush()
	if self.__flushScheduled then
		return
	end
	self.__flushScheduled = true

	task.delay(FLUSH_INTERVAL, function()
		self.__flushScheduled = false
		self:__flush()
	end)
end

function LogCapture:__flush()
	if #self.__pending == 0 then
		return
	end

	local batch = self.__pending
	self.__pending = {}

	-- Feedback is best-effort: a failed POST (e.g. the session dropping) must
	-- never disturb the live sync, and we deliberately don't log the failure
	-- here, since that output would be captured right back.
	self.__apiContext:feedback(batch):catch(function() end)
end

function LogCapture:stop()
	if self.__connection then
		self.__connection:Disconnect()
		self.__connection = nil
	end
	self.__pending = {}
	self.__flushScheduled = false
end

return LogCapture
