/*
 * FreeRDP dynamic virtual channel add-in for Teams media optimization.
 *
 * Loaded with /dvc:teams-redir. It listens on the two DVCs the Teams host opens,
 * com.microsoft.rdc.dvc.webrtc.1 and com.microsoft.rdc.dvc.cpc.1, copies each complete message
 * (FreeRDP reassembles DVC fragments before the callback) to the Rust core, and writes the core's
 * replies back. All protocol logic is in the core.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <freerdp/freerdp.h>
#include <freerdp/dvc.h>
#include <freerdp/settings.h>
#include <winpr/synch.h>
#include <winpr/wlog.h>

#include "tr_core.h"

#define TR_PLUGIN_NAME "teams-redir"
#define TR_NUM_CHANNELS 2
/* The host opens several webrtc.1 connections at once (three within 3 ms were seen), so each
 * accepted connection gets its own slot and id. */
#define TR_MAX_CONNS 32

static const char* const tr_channel_names[TR_NUM_CHANNELS + 1] = {
	NULL, "com.microsoft.rdc.dvc.webrtc.1", "com.microsoft.rdc.dvc.cpc.1"
};

typedef struct TrPlugin TrPlugin;

typedef struct
{
	IWTSListenerCallback iface;
	TrPlugin* plugin;
	int ch;
} TrListener;

typedef struct
{
	IWTSVirtualChannelCallback iface;
	TrPlugin* plugin;
	uint32_t id;
} TrChannel;

typedef struct
{
	uint32_t id; /* 0 = free */
	IWTSVirtualChannel* chan;
} TrConn;

struct TrPlugin
{
	IWTSPlugin iface;
	wLog* log;
	tr_core* core;
	TrListener listeners[TR_NUM_CHANNELS + 1];

	/* Guards `conns` and `next_id`. CRITICAL_SECTION is recursive, so closing a connection from
	 * inside a write (the core's panic path) does not deadlock against OnClose. */
	CRITICAL_SECTION lock;
	TrConn conns[TR_MAX_CONNS];
	uint32_t next_id;
};

/* Caller holds the lock. */
static TrConn* tr_find(TrPlugin* p, uint32_t id)
{
	if (id == 0)
		return NULL;
	for (int i = 0; i < TR_MAX_CONNS; i++)
		if (p->conns[i].id == id)
			return &p->conns[i];
	return NULL;
}

/* ---- callbacks the Rust core calls, from its dispatcher thread ---- */

static int tr_host_write(void* user, uint32_t conn, const uint8_t* data, size_t len)
{
	TrPlugin* p = user;
	int rc = -1;

	if (len > ULONG_MAX)
		return -1;

	/* The lock is held across Write so OnClose cannot free the channel underneath it. Whether
	 * FreeRDP accepts a DVC write from a thread other than its channel thread is an open question
	 * (plan §2.3); if it does not, this is where writes get queued to that thread instead. */
	EnterCriticalSection(&p->lock);
	TrConn* c = tr_find(p, conn);
	if (c)
		rc = (c->chan->Write(c->chan, (ULONG)len, data, NULL) == CHANNEL_RC_OK) ? 0 : -1;
	LeaveCriticalSection(&p->lock);
	return rc;
}

static void tr_host_close(void* user, uint32_t conn)
{
	TrPlugin* p = user;

	EnterCriticalSection(&p->lock);
	TrConn* c = tr_find(p, conn);
	if (c)
		(void)c->chan->Close(c->chan);
	LeaveCriticalSection(&p->lock);
}

static void tr_host_log(void* user, int level, const char* msg)
{
	TrPlugin* p = user;

	if (level < WLOG_TRACE || level > WLOG_ERROR)
		level = WLOG_INFO;
	WLog_Print(p->log, (DWORD)level, "%s", msg);
}

/* ---- session settings, passed to the core as JSON ---- */

static void tr_json_escape(char* dst, size_t cap, const char* src)
{
	size_t n = 0;

	for (; src && *src && n + 7 < cap; src++)
	{
		unsigned char c = (unsigned char)*src;
		if (c == '"' || c == '\\')
			dst[n++] = '\\', dst[n++] = (char)c;
		else if (c >= 0x20)
			dst[n++] = (char)c;
	}
	dst[n] = '\0';
}

static void tr_session_json(const rdpSettings* settings, char* out, size_t cap)
{
	char version[96];
	const UINT32 w = freerdp_settings_get_uint32(settings, FreeRDP_DesktopWidth);
	const UINT32 h = freerdp_settings_get_uint32(settings, FreeRDP_DesktopHeight);

	tr_json_escape(version, sizeof(version), freerdp_get_version_string());
	(void)snprintf(out, cap,
	               "{\"display\":{\"width\":%u,\"height\":%u},"
	               "\"client\":{\"name\":\"FreeRDP\",\"version\":\"%s\"}}",
	               (unsigned)w, (unsigned)h, version);
}

/* ---- DVC channel callbacks ---- */

static UINT tr_on_data_received(IWTSVirtualChannelCallback* cb, wStream* data)
{
	TrChannel* c = (TrChannel*)cb;

	if (!data)
		return CHANNEL_RC_OK;
	tr_core_data(c->plugin->core, c->id, Stream_ConstPointer(data), Stream_GetRemainingLength(data));
	return CHANNEL_RC_OK;
}

static UINT tr_on_open(IWTSVirtualChannelCallback* cb)
{
	(void)cb;
	return CHANNEL_RC_OK;
}

static UINT tr_on_close(IWTSVirtualChannelCallback* cb)
{
	TrChannel* c = (TrChannel*)cb;
	TrPlugin* p = c->plugin;

	EnterCriticalSection(&p->lock);
	TrConn* slot = tr_find(p, c->id);
	if (slot)
		memset(slot, 0, sizeof(*slot));
	LeaveCriticalSection(&p->lock);

	tr_core_channel_closed(p->core, c->id);
	free(c);
	return CHANNEL_RC_OK;
}

static UINT tr_on_new_connection(IWTSListenerCallback* lcb, IWTSVirtualChannel* channel,
                                 BYTE* Data, BOOL* pbAccept, IWTSVirtualChannelCallback** ppCallback)
{
	TrListener* l = (TrListener*)lcb;
	TrPlugin* p = l->plugin;
	TrConn* slot = NULL;
	TrChannel* c;

	(void)Data;
	*pbAccept = FALSE;

	c = calloc(1, sizeof(*c));
	if (!c)
		return CHANNEL_RC_NO_MEMORY;
	c->iface.OnDataReceived = tr_on_data_received;
	c->iface.OnOpen = tr_on_open;
	c->iface.OnClose = tr_on_close;
	c->plugin = p;

	EnterCriticalSection(&p->lock);
	for (int i = 0; i < TR_MAX_CONNS && !slot; i++)
		if (p->conns[i].id == 0)
			slot = &p->conns[i];
	if (slot)
	{
		c->id = ++p->next_id;
		slot->id = c->id;
		slot->chan = channel;
	}
	LeaveCriticalSection(&p->lock);

	if (!slot)
	{
		WLog_Print(p->log, WLOG_WARN, "%s: too many open connections, refusing one",
		           tr_channel_names[l->ch]);
		free(c);
		return CHANNEL_RC_OK;
	}

	tr_core_channel_opened(p->core, l->ch, c->id);
	*pbAccept = TRUE;
	*ppCallback = &c->iface;
	return CHANNEL_RC_OK;
}

/* ---- plugin lifecycle ---- */

static UINT tr_initialize(IWTSPlugin* plugin, IWTSVirtualChannelManager* mgr)
{
	TrPlugin* p = (TrPlugin*)plugin;
	char session[256];
	tr_host_api api = { 0 };

	api.user = p;
	api.write = tr_host_write;
	api.close = tr_host_close;
	api.log = tr_host_log;

	/* The settings are read through the entry points saved in DVCPluginEntry. */
	tr_session_json(p->iface.pInterface, session, sizeof(session));

	p->core = tr_core_new(&api, session);
	if (!p->core)
	{
		WLog_Print(p->log, WLOG_ERROR, "cannot create the Teams redirection core");
		return ERROR_INTERNAL_ERROR;
	}

	for (int ch = 1; ch <= TR_NUM_CHANNELS; ch++)
	{
		if (!tr_core_wants_channel(p->core, ch))
		{
			WLog_Print(p->log, WLOG_INFO, "%s: not listening (disabled by configuration)",
			           tr_channel_names[ch]);
			continue;
		}

		p->listeners[ch].iface.OnNewChannelConnection = tr_on_new_connection;
		p->listeners[ch].plugin = p;
		p->listeners[ch].ch = ch;

		const UINT rc = mgr->CreateListener(mgr, tr_channel_names[ch], 0, &p->listeners[ch].iface, NULL);
		if (rc != CHANNEL_RC_OK)
		{
			WLog_Print(p->log, WLOG_ERROR, "CreateListener(%s) failed: %u", tr_channel_names[ch], rc);
			return rc;
		}
		WLog_Print(p->log, WLOG_INFO, "%s: listening", tr_channel_names[ch]);
	}
	return CHANNEL_RC_OK;
}

static UINT tr_connected(IWTSPlugin* plugin)
{
	(void)plugin;
	return CHANNEL_RC_OK;
}

static UINT tr_disconnected(IWTSPlugin* plugin, DWORD code)
{
	(void)plugin;
	(void)code;
	return CHANNEL_RC_OK;
}

static UINT tr_terminated(IWTSPlugin* plugin)
{
	TrPlugin* p = (TrPlugin*)plugin;

	/* tr_core_free waits for the dispatcher thread, which may be inside tr_host_write, so the lock
	 * must not be held here. */
	tr_core_free(p->core);
	p->core = NULL;
	DeleteCriticalSection(&p->lock);
	free(p);
	return CHANNEL_RC_OK;
}

FREERDP_API UINT VCAPITYPE DVCPluginEntry(IDRDYNVC_ENTRY_POINTS* pEntryPoints);

UINT VCAPITYPE DVCPluginEntry(IDRDYNVC_ENTRY_POINTS* pEntryPoints)
{
	TrPlugin* p;

	if (pEntryPoints->GetPlugin(pEntryPoints, TR_PLUGIN_NAME))
		return CHANNEL_RC_ALREADY_INITIALIZED;

	p = calloc(1, sizeof(*p));
	if (!p)
		return CHANNEL_RC_NO_MEMORY;

	p->iface.Initialize = tr_initialize;
	p->iface.Connected = tr_connected;
	p->iface.Disconnected = tr_disconnected;
	p->iface.Terminated = tr_terminated;
	/* Initialize receives only the channel manager, so the settings ride in pInterface. */
	p->iface.pInterface = pEntryPoints->GetRdpSettings(pEntryPoints);
	p->log = WLog_Get("com.freerdp.channels.teams-redir");
	InitializeCriticalSection(&p->lock);

	const UINT rc = pEntryPoints->RegisterPlugin(pEntryPoints, TR_PLUGIN_NAME, &p->iface);
	if (rc != CHANNEL_RC_OK)
	{
		DeleteCriticalSection(&p->lock);
		free(p);
	}
	return rc;
}
