/*
 * redcam - PipeWire virtual camera that always displays a solid red frame.
 *
 * REFERENCE IMPLEMENTATION (C). A standalone second implementation that the
 * harness's oracle (redcam-test) verifies. The primary deliverable is the
 * Rust `pipewire-vircam` crate (see ../src); this file mirrors its format
 * coverage (RGBA, BGRA, BGR, RGB, YUY2, UYVY).
 *
 * Creates a PipeWire Video/Source node "redcam" with a fixed 1920x1080 @
 * 30 fps output. Every produced frame is solid red. Works with OBS and
 * Chrome via PipeWire.
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

/* Solid red (BT.709 limited-range YUV, 16-235). Same constants as the Rust
 * redcam.rs and the redcam-test oracle. */
#define RED_Y		63
#define RED_U		104
#define RED_V		240

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
	case SPA_VIDEO_FORMAT_YUY2:
	case SPA_VIDEO_FORMAT_UYVY:
		return 2;
	default:
		return 0;
	}
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
	case SPA_VIDEO_FORMAT_YUY2:
		/* Packed Y0 U Y1 V — two pixels per group. */
		for (x = 0; x < width; x += 2) {
			row[x * 2 + 0] = RED_Y;
			row[x * 2 + 1] = RED_U;
			row[x * 2 + 2] = RED_Y;
			row[x * 2 + 3] = RED_V;
		}
		break;
	case SPA_VIDEO_FORMAT_UYVY:
		/* Packed U Y V Y — two pixels per group. */
		for (x = 0; x < width; x += 2) {
			row[x * 2 + 0] = RED_U;
			row[x * 2 + 1] = RED_Y;
			row[x * 2 + 2] = RED_V;
			row[x * 2 + 3] = RED_Y;
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
	uint32_t w, h, stride, src_stride, n_bytes;
	int32_t y;

	if ((b = pw_stream_dequeue_buffer(data->stream)) == NULL) {
		pw_log_warn("out of buffers: %m");
		return;
	}
	buf = b->buffer;
	d = buf->datas;
	if (!d[0].data) {
		pw_stream_queue_buffer(data->stream, b);
		return;
	}

	w = data->format.size.width;
	h = data->format.size.height;
	if (w == 0 || h == 0) {
		/* Format not negotiated yet. */
		pw_stream_queue_buffer(data->stream, b);
		return;
	}

	/* Use the stride the consumer actually negotiated (from the buffer
	 * chunk), falling back to width * bpp. */
	stride = d[0].chunk->stride ? (uint32_t)d[0].chunk->stride
			      : w * bpp(data->format.format);
	src_stride = w * bpp(data->format.format);
	n_bytes = h * stride;

	if (src_stride == 0 ||
	    fill_red_row(row_buf, w, data->format.format) < 0) {
		pw_log_error("unsupported negotiated format %u",
			 data->format.format);
		pw_stream_queue_buffer(data->stream, b);
		return;
	}

	/* Copy row-by-row: source rows are src_stride wide, destination
	 * rows are stride wide (honouring negotiated alignment). */
	for (y = 0; y < (int32_t)h; y++)
		memcpy(d[0].data + (size_t)y * stride, row_buf, src_stride);

	/* Set chunk metadata after the fill. */
	d[0].chunk->offset = 0;
	d[0].chunk->size = (uint32_t)n_bytes;
	d[0].chunk->stride = (int32_t)stride;

	/* Fill the Header meta (PTS / seq).
	 * Use pw_stream_get_nsec for the PipeWire clock domain. */
	{
		struct spa_meta *m;
		if ((m = spa_buffer_find_meta(buf, SPA_META_Header)) != NULL &&
		    m->size >= sizeof(struct spa_meta_header)) {
			struct spa_meta_header *hdr = m->data;
			hdr->flags = 0;
			hdr->pts = pw_stream_get_nsec(data->stream);
			hdr->dts_offset = 0;
			hdr->seq = data->seq++;
		}
	}

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
			bool driving = pw_stream_is_driving(data->stream);
			bool lazy = pw_stream_is_lazy(data->stream);

			printf("driving:%d lazy:%d\n", (int)driving, (int)lazy);
			/* We drive the clock when the stream says we're driving
			 * AND the consumer isn't lazy (pulling on demand). In that
			 * case arm the timer at the negotiated frame rate. */
			if (driving && !lazy) {
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
	case PW_STREAM_STATE_ERROR:
		data->streaming = false;
		pw_loop_update_timer(loop, data->timer, NULL, NULL, false);
		/* "no target node available" is the normal idle state for a
		 * camera with no consumer (e.g. no browser tab yet). Keep the
		 * node registered so that Chrome / OBS can find it and connect.
		 * A different error would also be tolerated here: the node
		 * stays up and the user can kill it or let it retry. */
		if (error && strcmp(error, "no target node available") != 0)
			pw_log_error("stream error: %s", error);
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

	/* Reply with ParamBuffers + ParamMeta. */
	params[n_params++] = spa_pod_builder_add_object(&b,
		SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers,
		SPA_PARAM_BUFFERS_buffers,
		SPA_POD_CHOICE_RANGE_Int(4, 2, MAX_BUFFERS),
		SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(1),
		SPA_PARAM_BUFFERS_size,
		SPA_POD_Int((int32_t)(data->stride * data->format.size.height)),
		SPA_PARAM_BUFFERS_stride, SPA_POD_Int(data->stride));
	params[n_params++] = spa_pod_builder_add_object(&b,
		SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
		SPA_PARAM_META_type, SPA_POD_Id(SPA_META_Header),
		SPA_PARAM_META_size,
		SPA_POD_Int(sizeof(struct spa_meta_header)));
	pw_stream_update_params(data->stream, params, n_params);
}

static const struct pw_stream_events stream_events = {
	PW_VERSION_STREAM_EVENTS,
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
		SPA_VIDEO_FORMAT_YUY2,
		SPA_VIDEO_FORMAT_UYVY,
	};
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
		/* Video raw info POD with colorimetry (limited range, BT.601
		 * matrix, BT.709 transfer + primaries) matching a real webcam —
		 * Chrome requires this. */
		struct spa_video_info_raw info;
		memset(&info, 0, sizeof(info));
		info.format = fmts[i];
		info.size = SPA_RECTANGLE(WIDTH, HEIGHT);
		info.framerate.num = FPS;
		info.framerate.denom = 1;
		info.color_range = SPA_VIDEO_COLOR_RANGE_16_235;
		info.color_matrix = SPA_VIDEO_COLOR_MATRIX_BT601;
		info.transfer_function = SPA_VIDEO_TRANSFER_BT709;
		info.color_primaries = SPA_VIDEO_COLOR_PRIMARIES_BT709;
		params[n_params++] = spa_format_video_raw_build(&b,
				SPA_PARAM_EnumFormat, &info);
	}

	/* ask for a header meta (seq) alongside the video data */
	params[n_params++] = spa_pod_builder_add_object(&b,
		SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
		SPA_PARAM_META_type, SPA_POD_Id(SPA_META_Header),
		SPA_PARAM_META_size,
		SPA_POD_Int(sizeof(struct spa_meta_header)));

	pw_stream_add_listener(data.stream, &data.stream_hook,
			       &stream_events, &data);
	data.res = pw_stream_connect(data.stream, PW_DIRECTION_OUTPUT,
				     PW_ID_ANY,
					     PW_STREAM_FLAG_DRIVER |
					     PW_STREAM_FLAG_MAP_BUFFERS,
				     params, n_params);

cleanup:
	if (data.main_loop != NULL && data.res >= 0)
		pw_main_loop_run(data.main_loop);
	pw_main_loop_destroy(data.main_loop);
	pw_deinit();
	return data.res;
}
