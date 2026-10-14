// EmbeddingGemma 2 on llama.cpp, behind a few plain C calls for Rust (src/embed.rs). The structs llama.cpp and mtmd
// pass by value stay on this side, so Rust never has to mirror their layouts. The logic follows DigUp's Swift
// LlamaEmbedder: each input is its own sequence, `<bos> text <eos>` or `<bos> <|image> rows <image|> <eos>`, run
// through one llama_process call, mean-pooled and L2-normalized.
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "ggml-backend.h"
#include "llama.h"
#include "mtmd.h"
#include "mtmd-helper.h"

#define MAX_SEQUENCES 16

typedef struct dg_model {
    struct llama_model *model;
    struct llama_context *context;
    mtmd_context *media;
    struct llama_batch_ext *batch;
    const struct llama_vocab *vocab;
    int dim;        // the output projection: 768
    int row_size;   // the width of encoder rows fed to the text model
    int max_tokens;
    int sequences;  // in `batch`
    int tokens;     // in `batch`
    llama_pos position;
} dg_model;

static FILE *log_file = NULL;
static enum ggml_log_level log_level = GGML_LOG_LEVEL_INFO;

// Warnings and errors only (llama.cpp logs a lot at load time); CONT continues the previous message at its level.
static void log_callback(enum ggml_log_level level, const char *text, void *user_data) {
    (void)user_data;
    if (level != GGML_LOG_LEVEL_CONT) log_level = level;
    if (log_level < GGML_LOG_LEVEL_WARN || text == NULL) return;
    fputs(text, log_file ? log_file : stderr);
    fflush(log_file ? log_file : stderr);
}

// Once per process. `backends`: a folder with ggml's backend libraries (CPU variants, Vulkan, CUDA), or NULL.
// Returns the names of the backends that loaded, comma-separated, in `names`.
void dg_init(const char *backends, const char *log_path, char *names, int names_len) {
    if (log_path) log_file = fopen(log_path, "a");
    llama_log_set(log_callback, NULL);
    mtmd_helper_log_set(log_callback, NULL);
    if (backends && ggml_backend_dev_count() == 0) ggml_backend_load_all_from_path(backends);
    if (ggml_backend_dev_count() == 0) ggml_backend_load_all();
    llama_backend_init();
    if (names && names_len > 0) {
        names[0] = 0;
        for (size_t i = 0; i < ggml_backend_reg_count(); i++) {
            const char *name = ggml_backend_reg_name(ggml_backend_reg_get(i));
            if (strlen(names) + strlen(name) + 3 >= (size_t)names_len) break;
            if (names[0]) strcat(names, ", ");
            strcat(names, name);
        }
    }
}

void dg_close(dg_model *m) {
    if (!m) return;
    if (m->batch) llama_batch_ext_free(m->batch);
    if (m->media) mtmd_free(m->media);
    if (m->context) llama_free(m->context);
    if (m->model) llama_model_free(m->model);
    free(m);
}

// The first GPU's free memory in MB, or -1 when no GPU backend loaded.
int dg_gpu_free_mb(void) {
    for (size_t i = 0; i < ggml_backend_dev_count(); i++) {
        ggml_backend_dev_t device = ggml_backend_dev_get(i);
        if (ggml_backend_dev_type(device) != GGML_BACKEND_DEVICE_TYPE_GPU) continue;
        size_t free_bytes = 0, total_bytes = 0;
        ggml_backend_dev_memory(device, &free_bytes, &total_bytes);
        return (int)(free_bytes / (1024 * 1024));
    }
    return -1;
}

// `media_path` NULL: the text model alone (queries). One input must fit one micro-batch: 2048 tokens (the
// reference's cut, which also holds a 1120-token image or a 750-token 30 s clip); queries take 512.
// `use_gpu` 0: everything on the CPU (the GPU is missing or full).
dg_model *dg_open(const char *text_path, const char *media_path, int max_tokens, int threads, int use_gpu,
                  char *error, int error_len) {
    dg_model *m = calloc(1, sizeof(dg_model));
    struct llama_model_params model_params = llama_model_default_params();
    model_params.n_gpu_layers = use_gpu ? 999 : 0;
    m->model = llama_model_load_from_file(text_path, model_params);
    if (!m->model) {
        snprintf(error, error_len, "can't load %s", text_path);
        dg_close(m);
        return NULL;
    }
    struct llama_context_params params = llama_context_default_params();
    params.n_ctx = (uint32_t)max_tokens;
    params.n_batch = (uint32_t)max_tokens;
    params.n_ubatch = (uint32_t)max_tokens;
    params.n_seq_max = MAX_SEQUENCES;
    params.kv_unified = true;   // each input may use the whole context, as in llama-embedding
    params.embeddings = true;
    params.pooling_type = LLAMA_POOLING_TYPE_MEAN;
    params.n_threads = threads;
    params.n_threads_batch = threads;
    params.offload_kqv = use_gpu != 0;
    params.op_offload = use_gpu != 0;
    m->context = llama_init_from_model(m->model, params);
    if (!m->context) {
        snprintf(error, error_len, "can't create a llama.cpp context");
        dg_close(m);
        return NULL;
    }
    if (media_path) {
        struct mtmd_context_params media_params = mtmd_context_params_default();
        media_params.use_gpu = use_gpu != 0;
        media_params.print_timings = false;
        media_params.warmup = true;
        media_params.image_min_tokens = 1;   // never rescale: images arrive at their final size (embed.rs)
        media_params.image_max_tokens = 1120;
        media_params.n_threads = threads;
        m->media = mtmd_init_from_file(media_path, m->model, media_params);
        if (!m->media) {
            snprintf(error, error_len, "can't load %s", media_path);
            dg_close(m);
            return NULL;
        }
    }
    m->batch = llama_batch_ext_init(m->context);
    m->vocab = llama_model_get_vocab(m->model);
    m->dim = llama_model_n_embd_out(m->model);
    m->row_size = llama_model_n_embd_inp(m->model);
    m->max_tokens = max_tokens;
    return m;
}

int dg_dim(const dg_model *m) { return m->dim; }

int dg_audio_rate(const dg_model *m) { return m->media ? mtmd_get_audio_sample_rate(m->media) : 0; }

static void reset(dg_model *m) {
    llama_batch_ext_clear(m->batch);
    m->sequences = 0;
    m->tokens = 0;
}

static void begin(dg_model *m) {
    m->sequences += 1;
    m->position = 0;
}

static int place(dg_model *m, int32_t index) {
    if (index < 0) return -1;
    llama_pos pos[4] = {m->position, 0, 0, 0};
    if (!llama_batch_ext_set_pos(m->batch, index, pos) || !llama_batch_ext_set_output_embd(m->batch, index, true)) {
        return -1;
    }
    m->position += 1;
    m->tokens += 1;
    return 0;
}

static int add_token(dg_model *m, llama_token token) {
    return place(m, llama_batch_ext_add_token(m->batch, m->sequences - 1, token));
}

// Runs the batch; one L2-normalized vector per sequence into `out` (sequences × dim floats).
static int run(dg_model *m, float *out) {
    int status = llama_process(m->context, LLAMA_PROCESS_TYPE_ENCODE, m->batch);
    if (status != 0) {
        reset(m);
        return status;
    }
    for (int s = 0; s < m->sequences; s++) {
        const float *pooled = llama_get_embeddings_seq(m->context, s);
        if (!pooled) {
            reset(m);
            return -2;
        }
        double sum = 0;
        for (int i = 0; i < m->dim; i++) sum += (double)pooled[i] * pooled[i];
        float scale = sum > 0 ? (float)(1 / sqrt(sum)) : 0;
        for (int i = 0; i < m->dim; i++) out[s * m->dim + i] = pooled[i] * scale;
    }
    reset(m);
    return 0;
}

// `<bos> text <eos>`, cut to max_tokens the way transformers cuts (keeping both special tokens). Returns the count.
static int tokenize(dg_model *m, const char *text, llama_token **tokens) {
    int length = (int)strlen(text);
    int capacity = length + 8;
    *tokens = malloc(sizeof(llama_token) * capacity);
    int count = llama_tokenize(m->vocab, text, length, *tokens, capacity, true, true);
    if (count < 0) {
        capacity = -count;
        *tokens = realloc(*tokens, sizeof(llama_token) * capacity);
        count = llama_tokenize(m->vocab, text, length, *tokens, capacity, true, true);
    }
    if (count < 0) count = 0;
    if (count > m->max_tokens) {
        count = m->max_tokens;
        (*tokens)[count - 1] = llama_vocab_eos(m->vocab);
    }
    return count;
}

// `n` texts, up to 16 to a batch; `out`: n × dim floats. 0 on success.
int dg_embed_texts(dg_model *m, const char *const *texts, int n, float *out) {
    reset(m);
    int done = 0;   // vectors written
    for (int i = 0; i < n; i++) {
        llama_token *tokens = NULL;
        int count = tokenize(m, texts[i], &tokens);
        if (m->sequences == MAX_SEQUENCES || m->tokens + count > m->max_tokens) {
            int sequences = m->sequences;
            int status = run(m, out + (size_t)done * m->dim);
            if (status != 0) {
                free(tokens);
                return status;
            }
            done += sequences;
        }
        begin(m);
        for (int t = 0; t < count; t++) {
            if (add_token(m, tokens[t]) != 0) {
                free(tokens);
                reset(m);
                return -3;
            }
        }
        free(tokens);
    }
    if (m->sequences > 0) return run(m, out + (size_t)done * m->dim);
    return 0;
}

// One picture or one stretch of sound, already the size the model takes (Rust resizes pictures and cuts sound at
// 30 s), through the encoder into the text model. `out`: dim floats. 0 on success.
static int embed_bitmap(dg_model *m, mtmd_bitmap *bitmap, float *out) {
    if (!m->media) return -10;
    mtmd_input_chunks *chunks = mtmd_input_chunks_init();
    const char *marker = mtmd_default_marker();
    mtmd_input_text text = {marker, strlen(marker), true, true};
    const mtmd_bitmap *bitmaps[1] = {bitmap};
    int status = mtmd_tokenize(m->media, chunks, &text, bitmaps, 1);
    if (status != 0) {
        mtmd_input_chunks_free(chunks);
        return -20 - status;
    }
    reset(m);
    begin(m);
    for (size_t c = 0; c < mtmd_input_chunks_size(chunks) && status == 0; c++) {
        const mtmd_input_chunk *chunk = mtmd_input_chunks_get(chunks, c);
        if (mtmd_input_chunk_get_type(chunk) == MTMD_INPUT_CHUNK_TYPE_TEXT) {
            size_t count = 0;
            const llama_token *tokens = mtmd_input_chunk_get_tokens_text(chunk, &count);
            for (size_t t = 0; tokens && t < count && status == 0; t++) status = add_token(m, tokens[t]);
        } else {
            float *rows = NULL;
            if (mtmd_encode_chunk(m->media, chunk) != 0 || !(rows = mtmd_get_output_embd(m->media))) {
                status = -30;
                break;
            }
            size_t count = mtmd_input_chunk_get_n_tokens(chunk);
            for (size_t r = 0; r < count && status == 0; r++) {
                struct llama_embd embd = {rows + r * m->row_size, 1, (size_t)m->row_size};   // copied by the batch
                status = place(m, llama_batch_ext_add_embd(m->batch, m->sequences - 1, embd));
            }
        }
    }
    mtmd_input_chunks_free(chunks);
    if (status != 0) {
        reset(m);
        return status;
    }
    return run(m, out);
}

int dg_embed_image(dg_model *m, int width, int height, const unsigned char *rgb, float *out) {
    mtmd_bitmap *bitmap = mtmd_bitmap_init((uint32_t)width, (uint32_t)height, rgb);
    if (!bitmap) return -40;
    int status = embed_bitmap(m, bitmap, out);
    mtmd_bitmap_free(bitmap);
    return status;
}

int dg_embed_audio(dg_model *m, int samples, const float *pcm, float *out) {
    mtmd_bitmap *bitmap = mtmd_bitmap_init_from_audio((size_t)samples, pcm);
    if (!bitmap) return -40;
    int status = embed_bitmap(m, bitmap, out);
    mtmd_bitmap_free(bitmap);
    return status;
}
