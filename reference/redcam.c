/*
 * redcam - PipeWire virtual camera that always displays a solid red frame.
 *
 * REFERENCE IMPLEMENTATION (C). Lives in reference/ as a standalone, second
 * implementation that the harness's oracle (redcam-test) verifies. The
 * primary deliverable is the Rust `pipewire-vircam` crate (see ../src); this file
 * not kept in lockstep with the crate's format coverage (it offers only the
 * packed raw formats). Builds to the repo root as `redcam-c` (see Makefile).
 *
 * Creates a PipeWire node (MediaClass "Video/Source") with a fixed
 * 1920x1080 @ 30 fps output, offering packed raw formats RGBA, BGRA, BGR,
 * RGB. Every produced frame is solid red.
 *
 * Modelled after upstream PipeWire examples/video-src.c: a pw_stream with
 * direction OUTPUT, EnumFormat params, format negotiation in param_changed,
 * and a timer that drives frames at the negotiated framerate.
 */

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

#include <spa/param/video/raw.h>
#include <spa/param/video/raw-utils.h>
#include <pipewire/pipewire.h>


#define WIDTH		1920
#define HEIGHT		1080
#define FPS		30
#define MAX_BUFFERS	16

struct data {
	struct pw_main_loop	*main_loop;
	struct pw_context	*context;
	struct pw_core		*core;
	struct pw_stream	*stream;
	struct spa_hook		stream_hook;
	struct spa_source	*timer;

	struct spa_video_info_raw format;
	int32_t		stride;
	uint32_t		seq;
	bool		streaming;
	int		res;
};

static size_t bpp(uint32_t format)
{
	switch (format) {
	case SPA_VIDEO_FORMAT_RGB:
	case SPA_VIDEO_FORMAT_BGR:
		return 3;
	case SPA_VIDEO_FORMAT_RGBA:
	case SPA_VIDEO_FORMAT_BGRA:
		return 4;
	default:
		return 0;
	}
}

/* monotonic wall-clock in nanoseconds (CLOCK_MONOTONIC, the same domain
 * the v4l2 node stamps on captured buffers). */
static uint64_t now_ns(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

/* Write one solid red row of `width` pixels in `format` byte order. */
static int fill_red_row(uint8_t *row, uint32_t width, uint32_t format)
{
	uint32_t x;

	switch (format) {
	case SPA_VIDEO_FORMAT_RGB:
		for (x = 0; x < width; x++) {
			row[3*x + 0] = 0xff; /* R */
			row[3*x + 1] = 0x00; /* G */
			row[3*x + 2] = 0x00; /* B */
		}
		break;
	case SPA_VIDEO_FORMAT_BGR:
		for (x = 0; x < width; x++) {
			row[3*x + 0] = 0x00; /* B */
			row[3*x + 1] = 0x00; /* G */
			row[3*x + 2] = 0xff; /* R */
		}
		break;
	case SPA_VIDEO_FORMAT_RGBA:
		for (x = 0; x < width; x++) {
			row[4*x + 0] = 0xff; /* R */
			row[4*x + 1] = 0x00; /* G */
			row[4*x + 2] = 0x00; /* B */
			row[4*x + 3] = 0xff; /* A */
		}
		break;
	case SPA_VIDEO_FORMAT_BGRA:
		for (x = 0; x < width; x++) {
			row[4*x + 0] = 0x00; /* B */
			row[4*x + 1] = 0x00; /* G */
			row[4*x + 2] = 0xff; /* R */
			row[4*x + 3] = 0xff; /* A */
		}
		break;
	default:
		return -1;
	}
	return 0;
}

static void on_process(void *userdata)
{
	struct data *data = userdata;
	static uint8_t row_buf[WIDTH * 4];
	struct pw_buffer *b;
	struct spa_buffer *buf;
	struct spa_data *d;
	uint8_t *base;
	int32_t y;
	size_t row_len;

	if ((b = pw_stream_dequeue_buffer(data->stream)) == NULL) {
		pw_log_warn("out of buffers: %m");
		return;
	}
	buf = b->buffer;
	d = buf->datas;
	base = d[0].data;
	if (base == NULL)
		return;

	row_len = (size_t)data->format.size.width * bpp(data->format.format);
	if (row_len == 0 ||
	    fill_red_row(row_buf, data->format.size.width,
			 data->format.format) < 0) {
		pw_log_error("unsupported negotiated format %u",
			     data->format.format);
		return;
	}
	for (y = 0; y < (int32_t)data->format.size.height; y++)
		memcpy(base + y * data->stride, row_buf, row_len);

	{
		struct spa_meta *m;

		/* Fill the header meta with seq + pts. Best-effort: only
		 * write when the meta is actually present and fully
		 * sized. We never write a partial 32-byte struct, so a
		 * smaller (e.g. 8-byte) allocation can't be corrupted. */
		if ((m = spa_buffer_find_meta(buf, SPA_META_Header)) != NULL &&
		    m->size >= sizeof(struct spa_meta_header)) {
			struct spa_meta_header *h = m->data;
			h->flags = 0;
			h->offset = 0;
			h->seq = data->seq++;
			h->pts = (int64_t)now_ns();
			h->dts_offset = 0;
		}
	}

	d[0].chunk->offset = 0;
	d[0].chunk->size = (uint32_t)(data->format.size.height * data->stride);
	d[0].chunk->stride = data->stride;

	pw_stream_queue_buffer(data->stream, b);
}

static void on_stream_state_changed(void *userdata,
		enum pw_stream_state old_state,
		enum pw_stream_state state,
		const char *error)
{
	struct data *data = userdata;
	struct pw_loop *loop = pw_main_loop_get_loop(data->main_loop);

	(void)old_state;

	printf("stream state: \"%s\" %s\n",
	       pw_stream_state_as_string(state),
	       error ? error : "");

	switch (state) {
	case PW_STREAM_STATE_UNCONNECTED:
		data->streaming = false;
		pw_loop_update_timer(loop, data->timer, NULL, NULL, false);
		break;
	case PW_STREAM_STATE_PAUSED:
		data->streaming = false;
		printf("node id: %d\n", pw_stream_get_node_id(data->stream));
		pw_loop_update_timer(loop, data->timer, NULL, NULL, false);
		break;
	case PW_STREAM_STATE_STREAMING:
		{
			struct timespec timeout, interval;

			printf("driving:%d lazy:%d\n",
			       pw_stream_is_driving(data->stream),
			       pw_stream_is_lazy(data->stream));
			if (pw_stream_is_driving(data->stream) !=
			    pw_stream_is_lazy(data->stream)) {
				timeout.tv_sec = 0;
				timeout.tv_nsec = 1;
				interval.tv_sec = 0;
				interval.tv_nsec = SPA_NSEC_PER_SEC / FPS;
				data->streaming = true;
				pw_loop_update_timer(loop, data->timer,
					&timeout, &interval, false);
			} else {
				data->streaming = false;
				pw_loop_update_timer(loop, data->timer, NULL,
					NULL, false);
			}
		}
		break;
	default:
		break;
	}
}

static void on_stream_param_changed(void *userdata, uint32_t id,
		const struct spa_pod *param)
{
	struct data *data = userdata;
	uint8_t params_buffer[512];
	struct spa_pod_builder b = SPA_POD_BUILDER_INIT(params_buffer,
		sizeof(params_buffer));
	const struct spa_pod *params[2];
	uint32_t n_params = 0;

	if (param == NULL || id != SPA_PARAM_Format)
		return;

	if (spa_format_video_raw_parse(param, &data->format) < 0) {
		pw_log_error("failed to parse negotiated format");
		return;
	}
	data->stride = SPA_ROUND_UP_N(
		(int32_t)data->format.size.width * (int32_t)bpp(
			data->format.format), 4);
	printf("negotiated: format=%u %ux%u@%u/%u stride=%d\n",
	       data->format.format,
	       data->format.size.width, data->format.size.height,
	       data->format.framerate.num,
	       data->format.framerate.denom,
	       data->stride);

	/* Accept the negotiated format: reply with our buffer requirements. */
	params[n_params++] = spa_pod_builder_add_object(&b,
		SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers,
		SPA_PARAM_BUFFERS_buffers,
		SPA_POD_CHOICE_RANGE_Int(4, 2, MAX_BUFFERS),
		SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(1),
		SPA_PARAM_BUFFERS_size,
		SPA_POD_Int(data->stride * data->format.size.height),
		SPA_PARAM_BUFFERS_stride, SPA_POD_Int(data->stride));
	params[n_params++] = spa_pod_builder_add_object(&b,
		SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
		SPA_PARAM_META_type, SPA_POD_Int(SPA_META_Header),
		SPA_PARAM_META_size,
		SPA_POD_Int(sizeof(struct spa_meta_header)));
	pw_stream_update_params(data->stream, params, n_params);
}

static const struct pw_stream_events stream_events = {
	.process = on_process,
	.state_changed = on_stream_state_changed,
	.param_changed = on_stream_param_changed,
};

static void on_timeout(void *userdata, uint64_t expirations)
{
	struct data *data = userdata;

	(void)expirations;
	if (data->streaming)
		pw_stream_trigger_process(data->stream);
}

static void do_quit(void *userdata, int signal_number)
{
	struct data *data = userdata;

	(void)signal_number;
	pw_main_loop_quit(data->main_loop);
}

int main(int argc, char *argv[])
{
	struct data data = { 0 };
	const struct spa_pod *params[8];
	uint32_t n_params = 0;
	uint8_t buffer[4096];
	struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buffer,
		sizeof(buffer));
	const uint32_t fmts[] = {
		SPA_VIDEO_FORMAT_RGBA,
		SPA_VIDEO_FORMAT_BGRA,
		SPA_VIDEO_FORMAT_BGR,
		SPA_VIDEO_FORMAT_RGB,
	};
	const struct spa_rectangle rect = SPA_RECTANGLE(WIDTH, HEIGHT);
	size_t i;

	pw_init(&argc, &argv);
	/* Keep logs flowing even when stdout is redirected (tests, daemons). */
	setvbuf(stdout, NULL, _IONBF, 0);

	data.main_loop = pw_main_loop_new(NULL);
	if (data.main_loop == NULL) {
		fprintf(stderr, "can't create main loop\n");
		pw_deinit();
		return 1;
	}
	pw_loop_add_signal(pw_main_loop_get_loop(data.main_loop),
			   SIGINT, do_quit, &data);
	pw_loop_add_signal(pw_main_loop_get_loop(data.main_loop),
			   SIGTERM, do_quit, &data);
	data.timer = pw_loop_add_timer(pw_main_loop_get_loop(data.main_loop),
				       on_timeout, &data);
	data.context = pw_context_new(pw_main_loop_get_loop(data.main_loop),
				       NULL, 0);
	data.core = pw_context_connect(data.context, NULL, 0);
	if (data.core == NULL) {
		fprintf(stderr, "can't connect: %m\n");
		data.res = -errno;
		goto cleanup;
	}

	data.stream = pw_stream_new(data.core, "redcam",
		pw_properties_new(
			PW_KEY_NODE_NAME, "redcam",
			PW_KEY_NODE_DESCRIPTION, "Red Virtual Camera",
			PW_KEY_NODE_NICK, "Red Virtual Camera",
			PW_KEY_MEDIA_NAME, "Red Virtual Camera",
			PW_KEY_MEDIA_CLASS, "Video/Source",
			PW_KEY_MEDIA_TYPE, "Video",
			PW_KEY_MEDIA_CATEGORY, "Capture",
			PW_KEY_MEDIA_ROLE, "Camera",
			NULL));

	for (i = 0; i < sizeof(fmts) / sizeof(fmts[0]); i++) {
		params[n_params++] = spa_pod_builder_add_object(&b,
			SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat,
			SPA_FORMAT_mediaType,
			SPA_POD_Id(SPA_MEDIA_TYPE_video),
			SPA_FORMAT_mediaSubtype,
			SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw),
			SPA_FORMAT_VIDEO_format, SPA_POD_Id(fmts[i]),
			/* Plain (fixed) values, not choices: OBS's camera-portal
			 * requires a plain Rectangle for size and rejects range
			 * framerates; a plain fraction is accepted. */
			SPA_FORMAT_VIDEO_size,
			SPA_POD_Rectangle(&rect),
			SPA_FORMAT_VIDEO_framerate,
			SPA_POD_Fraction(&SPA_FRACTION(FPS, 1)));
	}

	/* ask for a header meta (seq) alongside the video data */
	params[n_params++] = spa_pod_builder_add_object(&b,
		SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
		SPA_PARAM_META_type, SPA_POD_Int(SPA_META_Header),
		SPA_PARAM_META_size,
		SPA_POD_Int(sizeof(struct spa_meta_header)));

	pw_stream_add_listener(data.stream, &data.stream_hook,
			       &stream_events, &data);
	data.res = pw_stream_connect(data.stream, PW_DIRECTION_OUTPUT,
				     PW_ID_ANY, PW_STREAM_FLAG_DRIVER,
				     params, n_params);

cleanup:
	if (data.main_loop != NULL && data.res >= 0)
		pw_main_loop_run(data.main_loop);
	pw_main_loop_destroy(data.main_loop);
	pw_deinit();
	return data.res;
}
