/*
 * Drives the built add-in through FreeRDP's DVC plugin interface with a mock channel manager:
 * entry point, listeners, accepting a channel, a complete message in, the reply written back,
 * close and teardown. The module is dlopen'd, so this also checks the exported entry point.
 */

#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <freerdp/dvc.h>
#include <freerdp/freerdp.h>
#include <freerdp/settings.h>
#include <winpr/stream.h>

#define CHECK(cond)                                                              \
	do                                                                           \
	{                                                                            \
		if (!(cond))                                                             \
		{                                                                        \
			fprintf(stderr, "%s:%d: check failed: %s\n", __FILE__, __LINE__, #cond); \
			exit(1);                                                             \
		}                                                                        \
	} while (0)

#define MAX_LISTENERS 4
#define MAX_WRITES 8

static struct
{
	char name[96];
	IWTSListenerCallback* cb;
} listeners[MAX_LISTENERS];
static int n_listeners;

typedef struct
{
	IWTSVirtualChannel iface;
	char written[MAX_WRITES][32768]; /* a createPeerConnection reply lists every codec */
	size_t written_len[MAX_WRITES];
	int n_written;
	int closed;
} MockChannel;

static UINT mock_write(IWTSVirtualChannel* ch, ULONG cb, const BYTE* buf, void* reserved)
{
	MockChannel* m = (MockChannel*)ch;
	(void)reserved;
	CHECK(m->n_written < MAX_WRITES && cb < sizeof(m->written[0]));
	memcpy(m->written[m->n_written], buf, cb);
	m->written[m->n_written][cb] = '\0';
	m->written_len[m->n_written++] = cb;
	return CHANNEL_RC_OK;
}

static UINT mock_close(IWTSVirtualChannel* ch)
{
	((MockChannel*)ch)->closed = 1;
	return CHANNEL_RC_OK;
}

static UINT mock_create_listener(IWTSVirtualChannelManager* mgr, const char* name, ULONG flags,
                                 IWTSListenerCallback* cb, IWTSListener** out)
{
	(void)mgr;
	(void)flags;
	(void)out;
	CHECK(n_listeners < MAX_LISTENERS);
	snprintf(listeners[n_listeners].name, sizeof(listeners[0].name), "%s", name);
	listeners[n_listeners++].cb = cb;
	return CHANNEL_RC_OK;
}

static IWTSPlugin* registered;
static rdpSettings* settings;

static UINT ep_register(IDRDYNVC_ENTRY_POINTS* ep, const char* name, IWTSPlugin* p)
{
	(void)ep;
	CHECK(strcmp(name, "teams-redir") == 0);
	registered = p;
	return CHANNEL_RC_OK;
}

static IWTSPlugin* ep_get_plugin(IDRDYNVC_ENTRY_POINTS* ep, const char* name)
{
	(void)ep;
	(void)name;
	return NULL;
}

static rdpSettings* ep_settings(IDRDYNVC_ENTRY_POINTS* ep)
{
	(void)ep;
	return settings;
}

/* The core replies from its own thread, so wait for the reply to arrive. */
static void wait_for_writes(MockChannel* m, int n)
{
	for (int i = 0; i < 200 && m->n_written < n; i++)
	{
		struct timespec ts = { 0, 10 * 1000 * 1000 };
		nanosleep(&ts, NULL);
	}
	CHECK(m->n_written >= n);
}

static IWTSVirtualChannelCallback* connect_channel(const char* name, MockChannel* m)
{
	IWTSVirtualChannelCallback* cb = NULL;
	BOOL accept = FALSE;

	memset(m, 0, sizeof(*m));
	m->iface.Write = mock_write;
	m->iface.Close = mock_close;
	for (int i = 0; i < n_listeners; i++)
	{
		if (strcmp(listeners[i].name, name) == 0)
		{
			CHECK(listeners[i].cb->OnNewChannelConnection(listeners[i].cb, &m->iface, NULL, &accept, &cb) ==
			      CHANNEL_RC_OK);
			CHECK(accept && cb);
			return cb;
		}
	}
	fprintf(stderr, "no listener for %s\n", name);
	exit(1);
}

static void send_message(IWTSVirtualChannelCallback* cb, const char* text, size_t len)
{
	wStream* s = Stream_New(NULL, len ? len : 1);
	CHECK(s);
	Stream_Write(s, text, len);
	Stream_SetPosition(s, 0);
	CHECK(cb->OnDataReceived(cb, s) == CHANNEL_RC_OK);
	Stream_Free(s, TRUE);
}

int main(int argc, char** argv)
{
	CHECK(argc == 2);
	void* lib = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
	if (!lib)
	{
		fprintf(stderr, "dlopen: %s\n", dlerror());
		return 1;
	}
	PDVC_PLUGIN_ENTRY entry = (PDVC_PLUGIN_ENTRY)dlsym(lib, "DVCPluginEntry");
	CHECK(entry);

	settings = freerdp_settings_new(0);
	CHECK(settings);
	CHECK(freerdp_settings_set_uint32(settings, FreeRDP_DesktopWidth, 1600));
	CHECK(freerdp_settings_set_uint32(settings, FreeRDP_DesktopHeight, 900));

	IDRDYNVC_ENTRY_POINTS ep = { 0 };
	ep.RegisterPlugin = ep_register;
	ep.GetPlugin = ep_get_plugin;
	ep.GetRdpSettings = ep_settings;
	CHECK(entry(&ep) == CHANNEL_RC_OK);
	CHECK(registered);

	IWTSVirtualChannelManager mgr = { 0 };
	mgr.CreateListener = mock_create_listener;
	CHECK(registered->Initialize(registered, &mgr) == CHANNEL_RC_OK);
	CHECK(n_listeners == 2);
	CHECK(registered->Connected(registered) == CHANNEL_RC_OK);

	/* cpc.1: VMHello in, ClientHello out (bare JSON, no NUL). */
	MockChannel cpc;
	IWTSVirtualChannelCallback* cpc_cb = connect_channel("com.microsoft.rdc.dvc.cpc.1", &cpc);
	const char* hello = "{\"contentType\":\"scpie\",\"messageName\":\"VMHello\",\"requestId\":11}";
	send_message(cpc_cb, hello, strlen(hello));
	wait_for_writes(&cpc, 1);
	CHECK(strstr(cpc.written[0], "\"messageName\":\"ClientHello\"") != NULL);
	CHECK(strstr(cpc.written[0], "\"requestId\":11") != NULL);
	CHECK(strstr(cpc.written[0], "\"rdClientProcuctName\":\"FreeRDP\"") != NULL);
	CHECK(cpc.written[0][cpc.written_len[0] - 1] == '}');

	/* webrtc.1: setVersionInfo is a notification (no rpcCallId) with its payload at the top level,
	 * as a real host sends it. It gets one NUL-terminated sessioninfo event and no response. */
	MockChannel web;
	IWTSVirtualChannelCallback* web_cb = connect_channel("com.microsoft.rdc.dvc.webrtc.1", &web);
	const char req[] = "{\"rpcObjectType\":\"RDWebRTCRedirector\",\"rpcName\":\"setVersionInfo\","
	                   "\"version\":{\"shim\":\"1.1.2602.23001\"},\"session\":{\"remoteApp\":false},\"features\":{}}";
	send_message(web_cb, req, sizeof(req)); /* includes the NUL */
	wait_for_writes(&web, 1);
	CHECK(web.written[0][web.written_len[0] - 1] == '\0');
	CHECK(strstr(web.written[0], "\"rpcEventName\":\"sessioninfo\"") != NULL);
	CHECK(strstr(web.written[0], "\"width\":1600") != NULL);
	CHECK(strstr(web.written[0], "\"height\":900") != NULL);
	CHECK(strstr(web.written[0], "\"rpcObjectId\"") == NULL);

	/* The host opens several webrtc.1 connections at once; each is accepted and answered on its
	 * own connection, and replies are not mixed up. */
	MockChannel web2;
	IWTSVirtualChannelCallback* web2_cb = connect_channel("com.microsoft.rdc.dvc.webrtc.1", &web2);
	const char call[] = "{\"rpcObjectType\":\"RTCPeerConnection\",\"rpcName\":\"addTrack\",\"rpcObjectId\":2,"
	                    "\"rpcCallId\":9,\"rpcArgs\":[{\"trackRpcObjectId\":7}]}";
	send_message(web2_cb, call, sizeof(call));
	wait_for_writes(&web2, 1);
	CHECK(strstr(web2.written[0], "\"rpcCallId\":9") != NULL);
	CHECK(strstr(web2.written[0], "\"hr\":-2147024846") != NULL);
	CHECK(web.n_written == 1); /* untouched by the other connection's traffic */

	/* A peer connection goes through the real media engine in this process, next to FreeRDP's own
	 * OpenSSL: the reply carries libwebrtc's audio codecs and an offer comes back from it. */
	const char create[] = "{\"rpcObjectType\":\"RTCPeerConnection\",\"rpcName\":\"createPeerConnection\","
	                      "\"rpcObjectId\":3,\"rpcCallId\":10,\"rpcArgs\":[{\"sdpSemantics\":\"unified-plan\"},{}]}";
	send_message(web2_cb, create, sizeof(create));
	wait_for_writes(&web2, 2);
	CHECK(strstr(web2.written[1], "\"rpcCallId\":10") != NULL);
	CHECK(strstr(web2.written[1], "audio/opus") != NULL);
	CHECK(strstr(web2.written[1], "\"hr\":0") != NULL);
	const char offer[] = "{\"rpcObjectType\":\"RTCPeerConnection\",\"rpcName\":\"createOffer\","
	                     "\"rpcObjectId\":3,\"rpcCallId\":11,\"rpcArgs\":[]}";
	send_message(web2_cb, offer, sizeof(offer));
	wait_for_writes(&web2, 3);
	CHECK(strstr(web2.written[2], "\"rpcCallId\":11") != NULL);
	CHECK(strstr(web2.written[2], "\"type\":\"offer\"") != NULL);

	/* Close all; afterwards the channel can be accepted again. OnClose frees the callback. */
	CHECK(web_cb->OnClose(web_cb) == CHANNEL_RC_OK);
	CHECK(web2_cb->OnClose(web2_cb) == CHANNEL_RC_OK);
	CHECK(cpc_cb->OnClose(cpc_cb) == CHANNEL_RC_OK);
	MockChannel again;
	IWTSVirtualChannelCallback* again_cb = connect_channel("com.microsoft.rdc.dvc.webrtc.1", &again);
	CHECK(again_cb->OnClose(again_cb) == CHANNEL_RC_OK);

	CHECK(registered->Disconnected(registered, 0) == CHANNEL_RC_OK);
	CHECK(registered->Terminated(registered) == CHANNEL_RC_OK); /* joins the dispatcher thread */

	freerdp_settings_free(settings);
	dlclose(lib);
	puts("shim_test: ok");
	return 0;
}
