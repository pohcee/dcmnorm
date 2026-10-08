// C ABI bridge over the vendored OpenHTJ2K C++ API (vendor/openhtj2k).
//
// build.rs compiles the whole library plus this file once per CPU variant. Every entry point's
// name carries the variant (DCMNORM_HTJ2K_VARIANT, e.g. dcmnorm_htj2k_base_decode /
// dcmnorm_htj2k_v3_decode) and is the only symbol left with default visibility, so two copies
// of OpenHTJ2K can live in one binary without their (largely un-namespaced) symbols colliding -
// see build.rs. No C++ exception may cross this boundary: every entry point catches everything
// and reports it through `error_message` instead.

#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <exception>
#include <string>
#include <vector>

#include "decoder.hpp"
#include "encoder.hpp"

#ifndef DCMNORM_HTJ2K_VARIANT
  #error "DCMNORM_HTJ2K_VARIANT must be defined by build.rs"
#endif

#define DCMNORM_CAT2(a, b, c) a##b##c
#define DCMNORM_CAT(a, b, c) DCMNORM_CAT2(a, b, c)
#define DCMNORM_HTJ2K_FN(name) DCMNORM_CAT(dcmnorm_htj2k_, DCMNORM_HTJ2K_VARIANT, _##name)
#define DCMNORM_HTJ2K_API extern "C" __attribute__((visibility("default")))

namespace {

constexpr uint8_t kNoQfactor = 0xFF;

char *duplicate_message(const std::string &message) {
  char *copy = static_cast<char *>(std::malloc(message.size() + 1));
  if (copy != nullptr) {
    std::memcpy(copy, message.c_str(), message.size() + 1);
  }
  return copy;
}

void set_error(char **error_message, const std::string &message) {
  if (error_message != nullptr) {
    *error_message = duplicate_message(message);
  }
}

// Mirrors the reference encoder app's default of 5 decomposition levels, reduced for images too
// small to support that many (each level halves both dimensions).
uint8_t decomposition_levels_for(uint32_t width, uint32_t height) {
  uint32_t smallest = width < height ? width : height;
  uint8_t levels    = 0;
  while (levels < 5 && smallest >= 2) {
    smallest = (smallest + 1) / 2;
    ++levels;
  }
  return levels;
}

}  // namespace

// Decoded image: one int32 plane per component, each `widths[c] * heights[c]` samples. Planes are
// allocated by OpenHTJ2K (new[]) and released by the matching free_image entry point.
struct dcmnorm_htj2k_image {
  uint16_t num_components;
  int32_t **planes;
  uint32_t *widths;
  uint32_t *heights;
  uint8_t *depths;
  uint8_t *is_signed;
};

DCMNORM_HTJ2K_API int DCMNORM_HTJ2K_FN(decode)(const uint8_t *codestream, size_t length,
                                               uint32_t num_threads, dcmnorm_htj2k_image *out,
                                               char **error_message) {
  if (out == nullptr || codestream == nullptr || length == 0) {
    set_error(error_message, "empty HTJ2K codestream");
    return 1;
  }
  std::memset(out, 0, sizeof(*out));
  std::vector<int32_t *> planes;
  try {
    open_htj2k::openhtj2k_decoder decoder(codestream, length, 0, num_threads);
    decoder.parse();
    std::vector<uint32_t> widths, heights;
    std::vector<uint8_t> depths;
    std::vector<bool> is_signed;
    decoder.invoke_line_based(planes, widths, heights, depths, is_signed);

    const size_t nc     = planes.size();
    out->num_components = static_cast<uint16_t>(nc);
    out->planes         = static_cast<int32_t **>(std::calloc(nc, sizeof(int32_t *)));
    out->widths         = static_cast<uint32_t *>(std::calloc(nc, sizeof(uint32_t)));
    out->heights        = static_cast<uint32_t *>(std::calloc(nc, sizeof(uint32_t)));
    out->depths         = static_cast<uint8_t *>(std::calloc(nc, sizeof(uint8_t)));
    out->is_signed      = static_cast<uint8_t *>(std::calloc(nc, sizeof(uint8_t)));
    if (!out->planes || !out->widths || !out->heights || !out->depths || !out->is_signed) {
      throw std::bad_alloc();
    }
    for (size_t c = 0; c < nc; ++c) {
      out->planes[c]    = planes[c];
      out->widths[c]    = widths[c];
      out->heights[c]   = heights[c];
      out->depths[c]    = depths[c];
      out->is_signed[c] = is_signed[c] ? 1 : 0;
    }
    return 0;
  } catch (const std::exception &error) {
    for (int32_t *plane : planes) delete[] plane;
    std::free(out->planes);
    std::free(out->widths);
    std::free(out->heights);
    std::free(out->depths);
    std::free(out->is_signed);
    std::memset(out, 0, sizeof(*out));
    // OpenHTJ2K signals many failures with a bare std::exception (its message lands on stdout).
    const char *what = error.what();
    set_error(error_message, std::string("OpenHTJ2K decode failed: ") +
                                 (what && *what && std::strcmp(what, "std::exception") != 0
                                      ? what
                                      : "invalid or unsupported codestream"));
    return 1;
  } catch (...) {
    for (int32_t *plane : planes) delete[] plane;
    set_error(error_message, "OpenHTJ2K decode failed: unknown error");
    return 1;
  }
}

// Header facts reported by decode_into, so the caller can tell a layout mismatch apart from a
// genuine decode failure.
struct dcmnorm_htj2k_info {
  uint16_t num_components;
  uint32_t width;
  uint32_t height;
  uint8_t depth;
  uint8_t is_signed;
};

// Streams the decoded rows straight into `out`, laid out as DICOM native pixel data: samples
// interleaved per pixel, `bytes_per_sample` (1 or 2) little-endian bytes each, low bits of the
// two's-complement value for signed data. Roughly twice as fast as decode() for a large image (no
// full-size int32 planes to allocate, fill and convert). Returns 2 - with `info` filled in and
// nothing decoded - when the codestream's components/dimensions don't match the expected ones
// (or components differ in size or depth); the caller then falls back to decode().
DCMNORM_HTJ2K_API int DCMNORM_HTJ2K_FN(decode_into)(const uint8_t *codestream, size_t length,
                                                    uint32_t num_threads, uint8_t *out, size_t out_len,
                                                    uint32_t bytes_per_sample, uint16_t expected_components,
                                                    uint32_t expected_width, uint32_t expected_height,
                                                    dcmnorm_htj2k_info *info, char **error_message) {
  if (codestream == nullptr || length == 0 || out == nullptr || info == nullptr ||
      (bytes_per_sample != 1 && bytes_per_sample != 2)) {
    set_error(error_message, "invalid HTJ2K decode_into parameters");
    return 1;
  }
  std::memset(info, 0, sizeof(*info));
  try {
    open_htj2k::openhtj2k_decoder decoder(codestream, length, 0, num_threads);
    decoder.parse();
    const uint16_t nc = decoder.get_num_component();
    info->num_components = nc;
    info->width          = nc ? decoder.get_component_width(0) : 0;
    info->height         = nc ? decoder.get_component_height(0) : 0;
    info->depth          = nc ? decoder.get_component_depth(0) : 0;
    info->is_signed      = nc && decoder.get_component_signedness(0) ? 1 : 0;
    bool uniform         = nc > 0;
    for (uint16_t c = 1; c < nc && uniform; ++c) {
      uniform = decoder.get_component_width(c) == info->width &&
                decoder.get_component_height(c) == info->height &&
                decoder.get_component_depth(c) == info->depth;
    }
    const size_t needed = static_cast<size_t>(info->width) * info->height * nc * bytes_per_sample;
    if (!uniform || nc != expected_components || info->width != expected_width ||
        info->height != expected_height || info->depth > 8 * bytes_per_sample || needed > out_len) {
      return 2;
    }

    const size_t row_stride = static_cast<size_t>(info->width) * nc * bytes_per_sample;
    const uint32_t width    = info->width;
    std::vector<uint32_t> widths, heights;
    std::vector<uint8_t> depths;
    std::vector<bool> is_signed;
    decoder.invoke_line_based_stream(
        [&](uint32_t y, int32_t *const *rows, uint16_t row_nc) {
          if (y >= info->height || row_nc != nc) return;
          uint8_t *dst = out + static_cast<size_t>(y) * row_stride;
          if (bytes_per_sample == 2) {
            for (uint16_t c = 0; c < nc; ++c) {
              const int32_t *src = rows[c];
              uint8_t *d         = dst + c * 2;
              for (uint32_t x = 0; x < width; ++x, d += 2 * nc) {
                const uint32_t v = static_cast<uint32_t>(src[x]);
                d[0]             = static_cast<uint8_t>(v);
                d[1]             = static_cast<uint8_t>(v >> 8);
              }
            }
          } else {
            for (uint16_t c = 0; c < nc; ++c) {
              const int32_t *src = rows[c];
              uint8_t *d         = dst + c;
              for (uint32_t x = 0; x < width; ++x, d += nc) *d = static_cast<uint8_t>(src[x]);
            }
          }
        },
        widths, heights, depths, is_signed);
    return 0;
  } catch (const std::exception &error) {
    const char *what = error.what();
    set_error(error_message, std::string("OpenHTJ2K decode failed: ") +
                                 (what && *what && std::strcmp(what, "std::exception") != 0
                                      ? what
                                      : "invalid or unsupported codestream"));
    return 1;
  } catch (...) {
    set_error(error_message, "OpenHTJ2K decode failed: unknown error");
    return 1;
  }
}

DCMNORM_HTJ2K_API void DCMNORM_HTJ2K_FN(free_image)(dcmnorm_htj2k_image *image) {
  if (image == nullptr) return;
  for (uint16_t c = 0; c < image->num_components; ++c) {
    if (image->planes != nullptr) delete[] image->planes[c];
  }
  std::free(image->planes);
  std::free(image->widths);
  std::free(image->heights);
  std::free(image->depths);
  std::free(image->is_signed);
  std::memset(image, 0, sizeof(*image));
}

// Lossless (reversible 5/3, single quality layer, 64x64 HT code-blocks) encode of planar int32
// samples. No multi-component transform is applied, so a 3-component image stays RGB - the
// DICOM PhotometricInterpretation doesn't have to change to YBR_RCT. `rpcl` selects RPCL
// progression instead of the default LRCP.
DCMNORM_HTJ2K_API int DCMNORM_HTJ2K_FN(encode)(const int32_t *const *planes, uint16_t num_components,
                                               uint32_t width, uint32_t height, uint8_t bit_depth,
                                               int is_signed, int rpcl, uint32_t num_threads,
                                               uint8_t **out_data, size_t *out_len,
                                               char **error_message) {
  if (out_data == nullptr || out_len == nullptr || planes == nullptr || num_components == 0 ||
      width == 0 || height == 0 || bit_depth == 0 || bit_depth > 16) {
    set_error(error_message, "invalid HTJ2K encode parameters");
    return 1;
  }
  *out_data = nullptr;
  *out_len  = 0;
  try {
    open_htj2k::siz_params siz;
    siz.Rsiz   = 0;
    siz.Xsiz   = width;
    siz.Ysiz   = height;
    siz.XOsiz  = 0;
    siz.YOsiz  = 0;
    siz.XTsiz  = width;
    siz.YTsiz  = height;
    siz.XTOsiz = 0;
    siz.YTOsiz = 0;
    siz.Csiz   = num_components;
    for (uint16_t c = 0; c < num_components; ++c) {
      siz.Ssiz.push_back(static_cast<uint8_t>((bit_depth - 1) | (is_signed ? 0x80 : 0x00)));
      siz.XRsiz.push_back(1);
      siz.YRsiz.push_back(1);
    }

    open_htj2k::cod_params cod;
    cod.blkwidth          = 4;  // log2(64) - 2
    cod.blkheight         = 4;
    cod.is_max_precincts  = true;
    cod.use_SOP           = false;
    cod.use_EPH           = false;
    cod.progression_order = rpcl ? 2 : 0;
    cod.number_of_layers  = 1;
    cod.use_color_trafo   = 0;
    cod.dwt_levels        = decomposition_levels_for(width, height);
    cod.codeblock_style   = 0x40;  // HT
    cod.transformation    = 1;     // reversible 5/3

    open_htj2k::qcd_params qcd{};
    qcd.is_derived          = false;
    qcd.number_of_guardbits = 1;
    qcd.base_step           = 1.0 / static_cast<double>(1u << bit_depth);

    // Row-streaming encode (the path the reference CLI uses for PNM/PGX input). The buffered
    // invoke_line_based() path dereferences unallocated subband buffers in this OpenHTJ2K version
    // (null memcpy in fdwt_level_sink_planes_fn, reproduced under ASan for any image size).
    std::vector<uint8_t> output;
    std::vector<int32_t *> no_buffered_input;
    open_htj2k::openhtj2k_encoder encoder("", no_buffered_input, siz, cod, qcd, kNoQfactor, false, 0,
                                          num_threads);
    encoder.set_output_buffer(output);
    encoder.invoke_line_based_stream([&](uint32_t y, int32_t **rows, uint16_t nc) {
      for (uint16_t c = 0; c < nc && c < num_components; ++c) {
        std::memcpy(rows[c], planes[c] + static_cast<size_t>(y) * width, sizeof(int32_t) * width);
      }
    });

    if (output.empty()) {
      set_error(error_message, "OpenHTJ2K encode produced no output");
      return 1;
    }
    *out_data = static_cast<uint8_t *>(std::malloc(output.size()));
    if (*out_data == nullptr) throw std::bad_alloc();
    std::memcpy(*out_data, output.data(), output.size());
    *out_len = output.size();
    return 0;
  } catch (const std::exception &error) {
    const char *what = error.what();
    set_error(error_message, std::string("OpenHTJ2K encode failed: ") +
                                 (what && *what && std::strcmp(what, "std::exception") != 0
                                      ? what
                                      : "encoder rejected the input"));
    return 1;
  } catch (...) {
    set_error(error_message, "OpenHTJ2K encode failed: unknown error");
    return 1;
  }
}

DCMNORM_HTJ2K_API void DCMNORM_HTJ2K_FN(free_buffer)(void *buffer) { std::free(buffer); }
