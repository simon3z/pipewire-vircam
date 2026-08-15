/*
 * redcam-test - capture consumer + verifier (the harness oracle).
 *
 * INDEPENDENT ORACLE. This is the shared verifier the harness uses against
 * *any* producer (Rust `redcam` by default, or the C reference `redcam-c`
 * via `RED_BIN=redcam-c make test`). It is C but is *not* part of the Rust
 * crate; it lives in reference/ alongside the C reference producer.
 *
 * Opens a real PipeWire input stream on a given video source node, captures
 * N frames and asserts:
 *   - negotiated size is the requested size (default 1920x1080)
 *   - every pixel of every frame is solid red
 *   - frames are distinct (Header seq advances)
 *   - framerate is ~the requested fps (default 30, tolerance +-20%)
 *
 * Usage: redcam-test <node-id> [frames] [--format FMT] [--size WxH] [--fps F]
 *
 *   --format FMT  (optional) force negotiation to a single format: one of
 *                 rgba bgra bgrx rgbx bgr rgb i420 nv12 nv21 yuy2 uyvy grey.
 *                 When omitted, all formats are advertised and PipeWire picks
 *                 the first common one.
 *   --size WxH    (optional) the size to advertise and verify (default 1920x1080).
 *   --fps F       (optional) the framerate to advertise and verify, as a whole
 *                 number (F/1) or a fraction A/B (default 30).
 *
 * Exits 0 and prints PASS on success, prints FAIL with reason and exits 1
 * otherwise. Modelled after upstream examples/video-play.c.
 */

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <spa/param/video/raw.h>
#include <spa/param/video/raw-utils.h>
#include <pipewire/pipewire.h>

/* Defaults when --size/--fps are not given. */
#define DEFAULT_WIDTH		1920
#define DEFAULT_HEIGHT		1080
#define DEFAULT_FPS		30
/* Largest width the oracle will verify (row_buf is sized for this). */
#define MAX_TEST_WIDTH		3840
#define DEFAULT_FRAMES		30
#define TIMEOUT_SECONDS		15

struct data {
	struct pw_main_loop	*main_loop;
	struct pw_context	*context;
	struct pw_core		*core;
	struct pw_stream	*stream;
	uint32_t		target_id;
	struct spa_hook		stream_hook;
	struct spa_source	*timeout_timer;

	uint32_t	n_frames;
	uint32_t	frames_seen;

	bool		size_ok;
	bool		red_bad;
	bool		seq_bad;
	uint32_t	seq_frames;
	uint32_t	last_seq;
	bool		row_built;
	uint32_t	force_fmt; /* 0 = all, else single format */
	uint32_t	want_w, want_h; /* requested size */
	int32_t	want_fps_num, want_fps_denom; /* requested fps */

	struct spa_video_info_raw format;
	int32_t		stride;

	struct timespec	t0;
	struct timespec	t1;
	bool		done;
	int		res;
};

static uint8_t row_buf[MAX_TEST_WIDTH * 4];

/* Number of planes for a format (chroma in separate buffers). */
static int num_planes(uint32_t format)
{
	switch (format) {
	case SPA_VIDEO_FORMAT_I420:
		return 3;
	case SPA_VIDEO_FORMAT_NV12:
	case SPA_VIDEO_FORMAT_NV21:
		return 2;
	default:
		return 1;
	}
}

/* Bytes per pixel of the first (primary) plane. Used for the negotiated
 * stride and buffer size. */
static size_t plane0_bpp(uint32_t format)
{
	switch (format) {
	case SPA_VIDEO_FORMAT_RGB:
	case SPA_VIDEO_FORMAT_BGR:
		return 3;
	case SPA_VIDEO_FORMAT_RGBA:
	case SPA_VIDEO_FORMAT_BGRA:
	case SPA_VIDEO_FORMAT_RGBx:
	case SPA_VIDEO_FORMAT_BGRx:
		return 4;
	case SPA_VIDEO_FORMAT_I420:
	case SPA_VIDEO_FORMAT_NV12:
	case SPA_VIDEO_FORMAT_NV21:
		return 1; /* Y plane */
	case SPA_VIDEO_FORMAT_YUY2:
	case SPA_VIDEO_FORMAT_UYVY:
		return 2; /* packed 4:2:2 (2 bytes/pix) */
	case SPA_VIDEO_FORMAT_GRAY8:
		return 1;
	default:
		return 0;
	}
}

/* Solid-red constants (BT.709 limited range), matching redcam.rs. */
#define RED_Y   63
#define RED_U   104
#define RED_V   240

/* Solid red row for the negotiated format (byte order matters). */
static int fill_red_row(uint8_t *row, uint32_t width, uint32_t format)
{
	uint32_t x;

	switch (format) {
	case SPA_VIDEO_FORMAT_RGB:
		for (x = 0; x < width; x++) {
			row[3*x + 0] = 0xff;
			row[3*x + 1] = 0x00;
			row[3*x + 2] = 0x00;
		}
		break;
	case SPA_VIDEO_FORMAT_BGR:
		for (x = 0; x < width; x++) {
			row[3*x + 0] = 0x00;
			row[3*x + 1] = 0x00;
			row[3*x + 2] = 0xff;
		}
		break;
	case SPA_VIDEO_FORMAT_RGBA:
		for (x = 0; x < width; x++) {
			row[4*x + 0] = 0xff;
			row[4*x + 1] = 0x00;
			row[4*x + 2] = 0x00;
			row[4*x + 3] = 0xff;
		}
		break;
	case SPA_VIDEO_FORMAT_RGBx:
		for (x = 0; x < width; x++) {
			row[4*x + 0] = 0xff;
			row[4*x + 1] = 0x00;
			row[4*x + 2] = 0x00;
			row[4*x + 3] = 0x00;
		}
		break;
	case SPA_VIDEO_FORMAT_BGRx:
		for (x = 0; x < width; x++) {
			row[4*x + 0] = 0x00;
			row[4*x + 1] = 0x00;
			row[4*x + 2] = 0xff;
			row[4*x + 3] = 0x00;
		}
		break;
	case SPA_VIDEO_FORMAT_BGRA:
		for (x = 0; x < width; x++) {
			row[4*x + 0] = 0x00;
			row[4*x + 1] = 0x00;
			row[4*x + 2] = 0xff;
			row[4*x + 3] = 0xff;
		}
		break;
	case SPA_VIDEO_FORMAT_GRAY8:
		memset(row, RED_Y, width);
		break;
	case SPA_VIDEO_FORMAT_YUY2:
		/* 2 bytes per pixel, 4 bytes per 2 pixels (Y U Y V). */
		for (x = 0; x < width; x += 2) {
			row[2*x + 0] = RED_Y;
			row[2*x + 1] = RED_U;
			row[2*x + 2] = RED_Y;
			row[2*x + 3] = RED_V;
		}
		break;
	case SPA_VIDEO_FORMAT_UYVY:
		/* 2 bytes per pixel, 4 bytes per 2 pixels (U Y V Y). */
		for (x = 0; x < width; x += 2) {
			row[2*x + 0] = RED_U;
			row[2*x + 1] = RED_Y;
			row[2*x + 2] = RED_V;
			row[2*x + 3] = RED_Y;
		}
		break;
	default:
		return -1;
	}
	return 0;
}

static double elapsed_ns(const struct timespec *a, const struct timespec *b)
{
	return (double)(b->tv_sec - a->tv_sec) * 1e9 +
	       (double)(b->tv_nsec - a->tv_nsec);
}

static int check_plane(const struct spa_data *d, uint32_t format, int plane)
{
	const uint8_t *base = (const uint8_t *)d->data;
	int32_t stride = d->chunk->stride;
	uint32_t h, i, total;

	if (stride <= 0)
		return -1;
	h = d->chunk->size / (uint32_t)stride;
	total = (uint32_t)stride * h;

	switch (format) {
	case SPA_VIDEO_FORMAT_I420: {
			uint8_t expect = plane == 0 ? RED_Y :
						  plane == 1 ? RED_U : RED_V;
			for (i = 0; i < total; i++)
				if (base[i] != expect)
					return -1;
			return 0;
		}
	case SPA_VIDEO_FORMAT_NV12:
	case SPA_VIDEO_FORMAT_NV21: {
			if (plane == 0) {
				for (i = 0; i < total; i++)
					if (base[i] != RED_Y)
						return -1;
				return 0;
			}
			uint8_t a = (format == SPA_VIDEO_FORMAT_NV12) ? RED_U : RED_V;
			uint8_t b = (format == SPA_VIDEO_FORMAT_NV12) ? RED_V : RED_U;
			for (i = 0; i < total; i++) {
				uint8_t expect = (i & 1) ? b : a;
				if (base[i] != expect)
					return -1;
			}
			return 0;
		}
	default:
		return 0; /* single-plane packed formats handled by caller */
	}
}

static void check_frame(struct data *data, struct pw_buffer *b)
{
	struct spa_data *d = b->buffer->datas;
	int n_expect = num_planes(data->format.format);
	int p;

	if (!data->size_ok) {
		data->size_ok =
			data->format.size.width == data->want_w &&
			data->format.size.height == data->want_h &&
			b->buffer->n_datas == (uint32_t)n_expect &&
			d[0].chunk->size >= (uint32_t)data->format.size.height *
				(uint32_t)data->stride;
	}

	/* Verify every plane against the expected red pattern. */
	for (p = 0; p < n_expect && p < (int)b->buffer->n_datas; p++) {
		if (n_expect > 1) {
			if (check_plane(&d[p], data->format.format, p) != 0) {
				data->red_bad = true;
				return;
			}
		} else {
			const uint8_t *base = (const uint8_t *)d[p].data;
			int32_t stride = d[p].chunk->stride;
			int32_t h = d[p].chunk->size / (uint32_t)stride;
			if (!data->row_built) {
				if (fill_red_row(row_buf, data->format.size.width,
						 data->format.format) < 0) {
					printf("unsupported negotiated format %u\n",
						 data->format.format);
					data->red_bad = true;
					return;
				}
				data->row_built = true;
			}
			for (int32_t y = 0; y < h; y++) {
				size_t row_len = (size_t)data->format.size.width *
					plane0_bpp(data->format.format);
				if (stride <= 0 || (size_t)stride < row_len) {
					data->red_bad = true;
					return;
				}
				if (memcmp(base + y * stride, row_buf, row_len) != 0) {
					data->red_bad = true;
					return;
				}
			}
		}
	}

	{
		struct spa_meta *m;

		if ((m = spa_buffer_find_meta(b->buffer, SPA_META_Header)) != NULL &&
		    m->size >= 8) {
			struct spa_meta_header *h = m->data;
			data->seq_frames++;
			if (data->seq_frames > 1 && h->seq == data->last_seq)
				data->seq_bad = true;
			data->last_seq = h->seq;
		}
	}
}
static void finish(struct data *data)
{
	double fps;
	bool red_ok, seq_ok, pass;

	data->t1.tv_sec = 0;
	data->t1.tv_nsec = 0;
	clock_gettime(CLOCK_MONOTONIC, &data->t1);
	fps = (data->frames_seen > 1) ?
		(double)(data->frames_seen - 1) / (elapsed_ns(&data->t0, &data->t1) / 1e9)
		: 0.0;

	red_ok = !data->red_bad;
	/* The Header meta (which carries per-frame seq) is best-effort:
	 * PipeWire only negotiates it into the shared buffer when the
	 * graph agrees on it. The upstream video-src.c example likewise
	 * guards its seq write with a NULL check. So we only assert seq
	 * advancement when the meta was actually present. */
	bool seq_checked = data->seq_frames >= 2;
	bool seq_pass = !seq_checked ||
			(data->seq_frames == data->frames_seen && !data->seq_bad);
	seq_ok = seq_pass;
	/* Framerate: the measured fps must be within +/-20% of the
	 * requested fps (PipeWire's timer pacing is not perfectly exact). */
	double want = (double)data->want_fps_denom * (double)data->want_fps_num;
	double tol_lo = want * 0.8, tol_hi = want * 1.2;
	bool fps_ok = fps >= tol_lo && fps <= tol_hi;
	pass = data->frames_seen >= data->n_frames &&
	       data->size_ok && red_ok && seq_pass && fps_ok;

	printf("frames=%u/%u size_ok=%d red_ok=%d seq_ok=%d seq_frames=%u "
	       "fps=%.2f negotiated=%u %ux%u@%u/%u\n",
	       data->frames_seen, data->n_frames,
	       data->size_ok, red_ok, seq_ok, data->seq_frames,
	       fps, data->format.format,
	       data->format.size.width, data->format.size.height,
	       data->format.framerate.num,
	       data->format.framerate.denom);
	if (!data->size_ok)
		printf("FAIL: size is not %dx%d (or chunk undersized)\n",
		       data->want_w, data->want_h);
	if (!red_ok)
		printf("FAIL: not every frame is solid red\n");
	if (seq_checked) {
		if (!seq_ok)
			printf("FAIL: frame sequence did not advance on every frame\n");
	} else {
		printf("note: header meta not negotiated; seq check skipped\n");
	}
	if (!fps_ok)
		printf("FAIL: measured fps %.2f outside [%0.2f, %0.2f] (requested %d/%d)\n",
		       fps, tol_lo, tol_hi,
		       data->want_fps_num, data->want_fps_denom);
	printf("%s\n", pass ? "PASS" : "FAIL");
	data->res = pass ? 0 : 1;
	data->done = true;
	pw_main_loop_quit(data->main_loop);
}

static void on_process(void *userdata)
{
	struct data *data = userdata;
	struct pw_buffer *b;

	if ((b = pw_stream_dequeue_buffer(data->stream)) == NULL)
		return;
	if (b->buffer->datas[0].data == NULL) {
		pw_stream_queue_buffer(data->stream, b);
		return;
	}
	if (data->frames_seen == 0)
		clock_gettime(CLOCK_MONOTONIC, &data->t0);
	check_frame(data, b);
	data->frames_seen++;
	pw_stream_queue_buffer(data->stream, b);
	if (data->frames_seen >= data->n_frames)
		finish(data);
}

static void on_stream_state_changed(void *userdata,
		enum pw_stream_state old_state,
		enum pw_stream_state state,
		const char *error)
{
	struct data *data = userdata;

	(void)old_state;
	printf("stream state: \"%s\" %s\n",
	       pw_stream_state_as_string(state),
	       error ? error : "");
	/* because we started inactive, activate ourselves now */
	if (state == PW_STREAM_STATE_PAUSED) {
		printf("consumer node id: %d\n",
		       pw_stream_get_node_id(data->stream));
		pw_stream_set_active(data->stream, true);
	}
}

static void on_stream_param_changed(void *userdata, uint32_t id,
		const struct spa_pod *param)
{
	struct data *data = userdata;

	if (param == NULL || id != SPA_PARAM_Format)
		return;

	if (spa_format_video_raw_parse(param, &data->format) < 0) {
		fprintf(stderr, "failed to parse negotiated format\n");
		return;
	}
	data->stride = SPA_ROUND_UP_N(
		(int32_t)data->format.size.width * (int32_t)plane0_bpp(
			data->format.format), 4);
	printf("negotiated: format=%u %ux%u@%u/%u stride=%d\n",
	       data->format.format,
	       data->format.size.width, data->format.size.height,
	       data->format.framerate.num,
	       data->format.framerate.denom,
	       data->stride);

	/* reply with our buffer config so the core can finish
	 * negotiation and allocate buffers */
	{
		uint8_t pbuf[512];
		struct spa_pod_builder pb = SPA_POD_BUILDER_INIT(pbuf,
			sizeof(pbuf));
		const struct spa_pod *pparams[2];
		uint32_t psize = data->format.size.width *
			data->format.size.height * plane0_bpp(data->format.format);
		pparams[0] = spa_pod_builder_add_object(&pb,
			SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers,
			SPA_PARAM_BUFFERS_buffers,
			SPA_POD_CHOICE_RANGE_Int(8, 2, 16),
			SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(num_planes(data->format.format)),
			SPA_PARAM_BUFFERS_size, SPA_POD_Int(psize),
			SPA_PARAM_BUFFERS_stride, SPA_POD_Int(data->stride));
		pparams[1] = spa_pod_builder_add_object(&pb,
			SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
			SPA_PARAM_META_type, SPA_POD_Int(SPA_META_Header),
			SPA_PARAM_META_size,
			SPA_POD_Int(sizeof(struct spa_meta_header)));
		printf("replying with buffer config size=%u\n", psize);
		if (pw_stream_update_params(data->stream, pparams, 2) < 0)
			fprintf(stderr, "failed to update params\n");
	}
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
	if (!data->done) {
		printf("FAIL: timeout after %d s with only %u/%u frames\n",
		       TIMEOUT_SECONDS, data->frames_seen, data->n_frames);
		data->res = 1;
	}
	pw_main_loop_quit(data->main_loop);
}

static void do_quit(void *userdata, int signal_number)
{
	struct data *data = userdata;

	(void)signal_number;
	if (!data->done)
		data->res = 1;
	pw_main_loop_quit(data->main_loop);
}

int main(int argc, char *argv[])
{
	struct data data = { 0 };
	data.target_id = PW_ID_ANY;
	const struct spa_pod *params[32];
	uint32_t n_params = 0;
	uint8_t buffer[4096];
	struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buffer,
		sizeof(buffer));
	const uint32_t fmts[] = {
		SPA_VIDEO_FORMAT_RGBA,
		SPA_VIDEO_FORMAT_BGRA,
		SPA_VIDEO_FORMAT_BGRx,
		SPA_VIDEO_FORMAT_RGBx,
		SPA_VIDEO_FORMAT_BGR,
		SPA_VIDEO_FORMAT_RGB,
		SPA_VIDEO_FORMAT_I420,
		SPA_VIDEO_FORMAT_NV12,
		SPA_VIDEO_FORMAT_NV21,
		SPA_VIDEO_FORMAT_YUY2,
		SPA_VIDEO_FORMAT_UYVY,
		SPA_VIDEO_FORMAT_GRAY8,
	};
	struct timespec timeout = { TIMEOUT_SECONDS, 0 };
	size_t i;

	/* Defaults; --size/--fps/--format override before use. */
	data.want_w = DEFAULT_WIDTH;
	data.want_h = DEFAULT_HEIGHT;
	data.want_fps_num = DEFAULT_FPS;
	data.want_fps_denom = 1;
	data.force_fmt = 0;

	if (argc < 2) {
		fprintf(stderr,
			"usage: %s <node-id> [frames] [--format FMT] [--size WxH] [--fps F]\n",
			argv[0]);
		return 2;
	}
	data.n_frames = argc > 2 ? (uint32_t)strtoul(argv[2], NULL, 10)
				  : DEFAULT_FRAMES;
	if (data.n_frames == 0 || data.n_frames > 1024)
		data.n_frames = DEFAULT_FRAMES;

	/* Option parsing: --format FMT, --size WxH, --fps F (or A/B). */
	for (int i = 1; i < argc; i++) {
		if (strcmp(argv[i], "--format") == 0 && i + 1 < argc) {
			const char *f = argv[++i];
			if      (!strcmp(f, "i420")) data.force_fmt = SPA_VIDEO_FORMAT_I420;
			else if (!strcmp(f, "nv12")) data.force_fmt = SPA_VIDEO_FORMAT_NV12;
			else if (!strcmp(f, "nv21")) data.force_fmt = SPA_VIDEO_FORMAT_NV21;
			else if (!strcmp(f, "yuy2")) data.force_fmt = SPA_VIDEO_FORMAT_YUY2;
			else if (!strcmp(f, "uyvy")) data.force_fmt = SPA_VIDEO_FORMAT_UYVY;
			else if (!strcmp(f, "grey")) data.force_fmt = SPA_VIDEO_FORMAT_GRAY8;
			else if (!strcmp(f, "rgba")) data.force_fmt = SPA_VIDEO_FORMAT_RGBA;
			else if (!strcmp(f, "bgra")) data.force_fmt = SPA_VIDEO_FORMAT_BGRA;
			else if (!strcmp(f, "bgrx")) data.force_fmt = SPA_VIDEO_FORMAT_BGRx;
			else if (!strcmp(f, "rgbx")) data.force_fmt = SPA_VIDEO_FORMAT_RGBx;
			else if (!strcmp(f, "bgr"))  data.force_fmt = SPA_VIDEO_FORMAT_BGR;
			else if (!strcmp(f, "rgb"))  data.force_fmt = SPA_VIDEO_FORMAT_RGB;
			else { fprintf(stderr, "unknown format %s\n", f); return 2; }
		} else if (strcmp(argv[i], "--size") == 0 && i + 1 < argc) {
			char *slash = strchr(argv[++i], 'x');
			if (!slash) { fprintf(stderr, "bad --size (want WxH)\n"); return 2; }
			data.want_w = (uint32_t)strtol(argv[i], NULL, 10);
			data.want_h = (uint32_t)strtol(slash + 1, NULL, 10);
		} else if (strcmp(argv[i], "--fps") == 0 && i + 1 < argc) {
			char *slash = strchr(argv[++i], '/');
			if (slash) {
				data.want_fps_num = (int32_t)strtol(argv[i], NULL, 10);
				*slash = 0;
				data.want_fps_denom = (int32_t)strtol(slash + 1, NULL, 10);
			} else {
				data.want_fps_num = (int32_t)strtol(argv[i], NULL, 10);
				data.want_fps_denom = 1;
			}
		}
	}
	if (data.want_w == 0 || data.want_h == 0) {
		fprintf(stderr, "bad --size\n");
		return 2;
	}
	if (data.want_w > MAX_TEST_WIDTH || data.want_fps_num <= 0 ||
	    data.want_fps_denom <= 0) {
		fprintf(stderr, "bad --size/--fps (w>=%d or fps<=0)\n", MAX_TEST_WIDTH);
		return 2;
	}
	pw_init(&argc, &argv);
	setvbuf(stdout, NULL, _IONBF, 0);
	data.main_loop = pw_main_loop_new(NULL);
	if (data.main_loop == NULL) {
		fprintf(stderr, "can't create main loop\n");
		return 1;
	}
	pw_loop_add_signal(pw_main_loop_get_loop(data.main_loop),
			   SIGINT, do_quit, &data);
	pw_loop_add_signal(pw_main_loop_get_loop(data.main_loop),
			   SIGTERM, do_quit, &data);
	data.timeout_timer = pw_loop_add_timer(
		pw_main_loop_get_loop(data.main_loop), on_timeout, &data);
	pw_loop_update_timer(pw_main_loop_get_loop(data.main_loop),
			     data.timeout_timer, &timeout, NULL, false);
	data.context = pw_context_new(pw_main_loop_get_loop(data.main_loop),
				       NULL, 0);
	data.core = pw_context_connect(data.context, NULL, 0);
	if (data.core == NULL) {
		fprintf(stderr, "can't connect: %m\n");
		data.res = 1;
		goto cleanup;
	}

	/* numeric argument = raw node id (no session manager needed);
	 * string argument = target object name (session manager) */
	{
		bool is_num = argv[1][0] != '\0';
		for (const char *c = argv[1]; *c; c++)
			if (*c < '0' || *c > '9')
				is_num = false;
		if (is_num) {
			uint32_t target = (uint32_t)strtoul(argv[1], NULL, 10);
			data.target_id = target;
			data.stream = pw_stream_new(data.core, "redcam-test",
				NULL);
		} else {
			data.stream = pw_stream_new(data.core, "redcam-test",
				pw_properties_new(
					PW_KEY_TARGET_OBJECT, argv[1],
					NULL));
		}
	}

	/* Advertise the requested size and framerate for each (forced) format,
	 * so the producer has to negotiate exactly what we asked for. */
	const struct spa_rectangle rect = SPA_RECTANGLE(data.want_w, data.want_h);
	const struct spa_fraction fps = SPA_FRACTION(data.want_fps_num, data.want_fps_denom);
	for (i = 0; i < sizeof(fmts) / sizeof(fmts[0]); i++) {
		if (data.force_fmt && fmts[i] != data.force_fmt)
			continue;
		params[n_params++] = spa_pod_builder_add_object(&b,
			SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat,
			SPA_FORMAT_mediaType,
			SPA_POD_Id(SPA_MEDIA_TYPE_video),
			SPA_FORMAT_mediaSubtype,
			SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw),
			SPA_FORMAT_VIDEO_format, SPA_POD_Id(fmts[i]),
			SPA_FORMAT_VIDEO_size,
			SPA_POD_CHOICE_RANGE_Rectangle(&rect, &rect, &rect),
			SPA_FORMAT_VIDEO_framerate,
			SPA_POD_CHOICE_RANGE_Fraction(
				&fps,
				&SPA_FRACTION(1, 1),
				&fps));
	}

	/* ask for a header meta (seq) alongside the video data */
	params[n_params++] = spa_pod_builder_add_object(&b,
		SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
		SPA_PARAM_META_type, SPA_POD_Int(SPA_META_Header),
		SPA_PARAM_META_size,
		SPA_POD_Int(sizeof(struct spa_meta_header)));

	pw_stream_add_listener(data.stream, &data.stream_hook,
			       &stream_events, &data);
	data.res = pw_stream_connect(data.stream, PW_DIRECTION_INPUT,
				     data.target_id,
				     PW_STREAM_FLAG_AUTOCONNECT |
				     PW_STREAM_FLAG_INACTIVE |
				     PW_STREAM_FLAG_MAP_BUFFERS,
				     params, n_params);
	if (data.res < 0)
		goto cleanup;

cleanup:
	if (data.main_loop != NULL)
		pw_main_loop_run(data.main_loop);
	pw_main_loop_destroy(data.main_loop);
	pw_deinit();
	if (!data.done)
		printf("FAIL: exited without result (res=%d)\n", data.res);
	return data.res;
}
