#include <algorithm>
#include <cassert>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <stdexcept>
#include <string>
#include <vector>

#include "kdu_elementary.h"
#include "kdu_messaging.h"
#include "kdu_params.h"
#include "kdu_compressed.h"
#include "kdu_sample_processing.h"
#include "kdu_stripe_decompressor.h"
#include "kdu_stripe_compressor.h"

using namespace kdu_core;
using namespace kdu_supp;

namespace {

class throwing_kdu_message : public kdu_message {
public:
  void start_message() override { text.clear(); }
  void put_text(const char *string) override {
    if (string != nullptr) {
      text += string;
    }
  }
  void flush(bool end_of_message = false) override {
    if (end_of_message) {
      if (text.empty()) {
        text = "Kakadu operation failed";
      }
      throw std::runtime_error(text);
    }
  }

private:
  std::string text;
};

struct error_scope {
  throwing_kdu_message handler;
  error_scope() { kdu_customize_errors(&handler); }
  ~error_scope() {
    try {
      kdu_customize_errors(nullptr);
    } catch (...) {
    }
  }
};

class memory_source : public kdu_compressed_source {
public:
  memory_source(const uint8_t *data, size_t size)
      : data_(reinterpret_cast<const kdu_byte *>(data)), size_(size), pos_(0) {
    if (data == nullptr && size != 0) {
      throw std::runtime_error("null codestream pointer");
    }
  }

  int get_capabilities() override {
    return KDU_SOURCE_CAP_SEQUENTIAL | KDU_SOURCE_CAP_SEEKABLE | KDU_SOURCE_CAP_IN_MEMORY;
  }

  int read(kdu_byte *buf, int num_bytes) override {
    if (num_bytes <= 0 || buf == nullptr) {
      return 0;
    }
    const size_t remaining = (pos_ < size_) ? (size_ - pos_) : 0;
    const size_t requested = static_cast<size_t>(num_bytes);
    const size_t count = std::min(remaining, requested);
    if (count > 0) {
      std::memcpy(buf, data_ + pos_, count);
      pos_ += count;
    }
    return static_cast<int>(count);
  }

  bool seek(kdu_long offset) override {
    if (offset < 0) {
      return true;
    }
    const size_t target = static_cast<size_t>(offset);
    pos_ = std::min(target, size_);
    return true;
  }

  kdu_long get_pos() override {
    return static_cast<kdu_long>(pos_);
  }

  kdu_byte *access_memory(kdu_long &pos, kdu_byte *&lim) override {
    pos = static_cast<kdu_long>(pos_);
    lim = const_cast<kdu_byte *>(data_) + size_;
    return const_cast<kdu_byte *>(data_) + pos_;
  }

private:
  const kdu_byte *data_;
  size_t size_;
  size_t pos_;
};

class memory_target : public kdu_compressed_target {
public:
  bool write(const kdu_byte *buf, int num_bytes) override {
    if (num_bytes <= 0) {
      return true;
    }
    size_t end = pos_ + static_cast<size_t>(num_bytes);
    if (rewriting_ && end > data_.size()) {
      // Writing past the position `start_rewrite' backtracked from would corrupt data written
      // after it - not allowed (see `kdu_compressed_target::start_rewrite' docs).
      return false;
    }
    if (end > data_.size()) {
      data_.resize(end);
    }
    std::memcpy(data_.data() + pos_, buf, static_cast<size_t>(num_bytes));
    pos_ = end;
    return true;
  }

  bool start_rewrite(kdu_long backtrack) override {
    if (rewriting_ || backtrack < 0 || static_cast<size_t>(backtrack) > data_.size()) {
      return false;
    }
    saved_pos_ = pos_;
    pos_ = data_.size() - static_cast<size_t>(backtrack);
    rewriting_ = true;
    return true;
  }

  bool end_rewrite() override {
    if (!rewriting_) {
      return false;
    }
    pos_ = saved_pos_;
    rewriting_ = false;
    return true;
  }

  const std::vector<uint8_t> &data() const { return data_; }

private:
  std::vector<uint8_t> data_;
  size_t pos_ = 0;
  size_t saved_pos_ = 0;
  bool rewriting_ = false;
};

void set_error(char **error_message, const std::string &message) {
  if (error_message == nullptr) {
    return;
  }
  auto *buffer = static_cast<char *>(std::malloc(message.size() + 1));
  if (buffer == nullptr) {
    *error_message = nullptr;
    return;
  }
  std::memcpy(buffer, message.c_str(), message.size() + 1);
  *error_message = buffer;
}

void validate_common_args(int rows, int cols, int samples_per_pixel, int bits_stored) {
  if (rows <= 0 || cols <= 0) {
    throw std::runtime_error("rows and columns must be positive");
  }
  if (samples_per_pixel <= 0) {
    throw std::runtime_error("samples_per_pixel must be positive");
  }
  if (bits_stored <= 0 || bits_stored > 16) {
    throw std::runtime_error("Kakadu bridge currently supports 1..16 bits stored");
  }
}

std::vector<int> make_component_array(int value, int count) {
  const int padded_count = std::max(count, 3);
  return std::vector<int>(static_cast<size_t>(padded_count), value);
}

std::vector<uint8_t> decode_impl(
    const uint8_t *codestream,
    size_t codestream_len,
    int rows,
    int cols,
    int samples_per_pixel,
    int bits_stored,
    int is_signed) {
  validate_common_args(rows, cols, samples_per_pixel, bits_stored);
  if (codestream == nullptr && codestream_len != 0) {
    throw std::runtime_error("null codestream pointer");
  }

  memory_source input(codestream, codestream_len);

  kdu_codestream codestream_obj;
  codestream_obj.create(&input);
  codestream_obj.apply_input_restrictions(0, 0, 0, 0, nullptr, KDU_WANT_OUTPUT_COMPONENTS);
  codestream_obj.set_fast();

  const int component_count = codestream_obj.get_num_components(true);
  if (component_count != samples_per_pixel) {
    throw std::runtime_error("decoded component count does not match DICOM metadata");
  }
  for (int c = 0; c < component_count; ++c) {
    kdu_dims dims;
    codestream_obj.get_dims(c, dims, true);
    if ((dims.size.x != cols) || (dims.size.y != rows)) {
      throw std::runtime_error("decoded image dimensions do not match DICOM metadata");
    }
  }

  kdu_stripe_decompressor decompressor;
  decompressor.start(codestream_obj);

  auto stripe_heights = make_component_array(rows, samples_per_pixel);
  auto precisions = make_component_array(bits_stored, samples_per_pixel);

  std::vector<uint8_t> result;
  if (bits_stored <= 8) {
    result.resize(static_cast<size_t>(rows) * static_cast<size_t>(cols) * static_cast<size_t>(samples_per_pixel));
    decompressor.pull_stripe(result.data(), stripe_heights.data());
  } else {
    std::vector<kdu_int16> buffer(static_cast<size_t>(rows) * static_cast<size_t>(cols) * static_cast<size_t>(samples_per_pixel));
    std::unique_ptr<bool[]> signed_flags(new bool[static_cast<size_t>(samples_per_pixel)]);
    for (int i = 0; i < samples_per_pixel; ++i) {
      signed_flags[static_cast<size_t>(i)] = (is_signed != 0);
    }
    decompressor.pull_stripe(buffer.data(), stripe_heights.data(), nullptr, nullptr, nullptr,
                             precisions.data(), signed_flags.get(), nullptr, 0);
    result.resize(buffer.size() * 2);
    for (size_t i = 0; i < buffer.size(); ++i) {
      const uint16_t word = static_cast<uint16_t>(buffer[i]);
      result[i * 2] = static_cast<uint8_t>(word & 0xFF);
      result[i * 2 + 1] = static_cast<uint8_t>((word >> 8) & 0xFF);
    }
  }

  decompressor.finish();
  codestream_obj.destroy();
  return result;
}

std::vector<uint8_t> encode_impl(
    const uint8_t *pixel_data,
    size_t pixel_data_len,
    int rows,
    int cols,
    int samples_per_pixel,
    int bits_stored,
    int is_signed,
    int lossless,
    double lossy_compression_ratio) {
  validate_common_args(rows, cols, samples_per_pixel, bits_stored);
  const size_t bytes_per_sample = (bits_stored > 8) ? 2 : 1;
  const size_t expected_len = static_cast<size_t>(rows) * static_cast<size_t>(cols) *
      static_cast<size_t>(samples_per_pixel) * bytes_per_sample;
  if (pixel_data_len != expected_len) {
    throw std::runtime_error("pixel data length does not match rows/cols/samples_per_pixel/bits_stored");
  }
  if (pixel_data == nullptr) {
    throw std::runtime_error("null pixel data pointer");
  }

  siz_params siz;
  siz.set(Scomponents, 0, 0, samples_per_pixel);
  for (int c = 0; c < samples_per_pixel; ++c) {
    siz.set(Sdims, c, 0, rows);
    siz.set(Sdims, c, 1, cols);
    siz.set(Sprecision, c, 0, bits_stored);
    siz.set(Ssigned, c, 0, is_signed != 0);
  }
  kdu_params *siz_ref = &siz;
  siz_ref->finalize();

  memory_target target;
  kdu_codestream codestream_obj;
  codestream_obj.create(&siz, &target);
  codestream_obj.access_siz()->parse_string(lossless ? "Creversible=yes" : "Creversible=no");
  {
    // Kakadu's default of 5 wavelet decomposition levels needs enough margin in the smallest
    // image dimension that its coarsest subbands are still non-degenerate; unlike OpenJPEG (see
    // the sibling raw-FFI OpenJPEG encoder), Kakadu doesn't reject a mismatch here outright, it
    // silently produces a codestream that reconstructs to scrambled pixel data (verified
    // empirically). Scale levels down for small images rather than risk that.
    int smallest_dim = std::min(rows, cols);
    int levels = 0;
    while ((1 << (levels + 1)) <= smallest_dim && levels < 5) {
      ++levels;
    }
    char clevels[32];
    std::snprintf(clevels, sizeof(clevels), "Clevels=%d", levels);
    codestream_obj.access_siz()->parse_string(clevels);
  }
  codestream_obj.access_siz()->finalize_all();

  kdu_stripe_compressor compressor;
  kdu_long layer_size = 0;
  kdu_long *layer_sizes = nullptr;
  int num_layer_specs = 0;
  if (!lossless) {
    // `tcp_rates`-style ratio convention (matches the OpenJPEG backend for consistency): a
    // ratio of 10 means "compress to roughly 1/10th the uncompressed size".
    double ratio = (lossy_compression_ratio > 1.0) ? lossy_compression_ratio : 1.0;
    layer_size = static_cast<kdu_long>(static_cast<double>(expected_len) / ratio);
    if (layer_size < 1) {
      layer_size = 1;
    }
    layer_sizes = &layer_size;
    num_layer_specs = 1;
  }
  compressor.start(codestream_obj, num_layer_specs, layer_sizes);

  auto stripe_heights = make_component_array(rows, samples_per_pixel);
  auto precisions = make_component_array(bits_stored, samples_per_pixel);
  // `make_component_array` pads to at least 3 entries (matching `decode_impl`'s convention for
  // Kakadu's internal component-array expectations), but `push_stripe` must only be told about
  // the `samples_per_pixel` components that actually exist.
  stripe_heights.resize(static_cast<size_t>(samples_per_pixel));
  precisions.resize(static_cast<size_t>(samples_per_pixel));

  if (bits_stored <= 8) {
    compressor.push_stripe(const_cast<kdu_byte *>(pixel_data), stripe_heights.data(),
                            nullptr, nullptr, nullptr, precisions.data());
  } else {
    std::vector<kdu_int16> buffer(expected_len / 2);
    for (size_t i = 0; i < buffer.size(); ++i) {
      const uint16_t word = static_cast<uint16_t>(pixel_data[i * 2]) |
          (static_cast<uint16_t>(pixel_data[i * 2 + 1]) << 8);
      buffer[i] = static_cast<kdu_int16>(word);
    }
    std::unique_ptr<bool[]> signed_flags(new bool[static_cast<size_t>(samples_per_pixel)]);
    for (int i = 0; i < samples_per_pixel; ++i) {
      signed_flags[static_cast<size_t>(i)] = (is_signed != 0);
    }
    compressor.push_stripe(buffer.data(), stripe_heights.data(), nullptr, nullptr, nullptr,
                            precisions.data(), signed_flags.get());
  }

  compressor.finish();
  codestream_obj.destroy();
  return target.data();
}

} // namespace

extern "C" int dcmnorm_kakadu_decode(
    const uint8_t *codestream,
    size_t codestream_len,
    int rows,
    int cols,
    int samples_per_pixel,
    int bits_stored,
    int is_signed,
    uint8_t **out_data,
    size_t *out_len,
    char **error_message) {
  if (out_data == nullptr || out_len == nullptr) {
    set_error(error_message, "invalid output pointers passed to Kakadu decode");
    return 1;
  }
  *out_data = nullptr;
  *out_len = 0;
  if (error_message != nullptr) {
    *error_message = nullptr;
  }

  try {
    error_scope errors;
    std::vector<uint8_t> decoded = decode_impl(codestream, codestream_len, rows, cols, samples_per_pixel, bits_stored, is_signed);
    auto *buffer = static_cast<uint8_t *>(std::malloc(decoded.size()));
    if (buffer == nullptr && !decoded.empty()) {
      throw std::runtime_error("failed to allocate decoded output buffer");
    }
    if (!decoded.empty()) {
      std::memcpy(buffer, decoded.data(), decoded.size());
    }
    *out_data = buffer;
    *out_len = decoded.size();
    return 0;
  } catch (const std::exception &error) {
    set_error(error_message, error.what());
    return 1;
  } catch (...) {
    set_error(error_message, "unknown Kakadu decode failure");
    return 1;
  }
}

extern "C" int dcmnorm_kakadu_encode(
    const uint8_t *pixel_data,
    size_t pixel_data_len,
    int rows,
    int cols,
    int samples_per_pixel,
    int bits_stored,
    int is_signed,
    int lossless,
    double lossy_compression_ratio,
    uint8_t **out_data,
    size_t *out_len,
    char **error_message) {
  if (out_data == nullptr || out_len == nullptr) {
    set_error(error_message, "invalid output pointers passed to Kakadu encode");
    return 1;
  }
  *out_data = nullptr;
  *out_len = 0;
  if (error_message != nullptr) {
    *error_message = nullptr;
  }

  try {
    error_scope errors;
    std::vector<uint8_t> encoded = encode_impl(pixel_data, pixel_data_len, rows, cols,
        samples_per_pixel, bits_stored, is_signed, lossless, lossy_compression_ratio);
    auto *buffer = static_cast<uint8_t *>(std::malloc(encoded.size()));
    if (buffer == nullptr && !encoded.empty()) {
      throw std::runtime_error("failed to allocate encoded output buffer");
    }
    if (!encoded.empty()) {
      std::memcpy(buffer, encoded.data(), encoded.size());
    }
    *out_data = buffer;
    *out_len = encoded.size();
    return 0;
  } catch (const std::exception &error) {
    set_error(error_message, error.what());
    return 1;
  } catch (...) {
    set_error(error_message, "unknown Kakadu encode failure");
    return 1;
  }
}

extern "C" int dcmnorm_kakadu_supports_htj2k() {
  // Kakadu only gained HTJ2K (Part-15) support in v8.0. Versions before that don't just fail to
  // decode an HT-coded codestream cleanly - kdu_codestream::create() can hang indefinitely while
  // trying to interpret Part-15-only Scod/marker signaling it has no concept of, so callers must
  // check this *before* ever handing Kakadu an HTJ2K codestream, not rely on catching an error
  // from the decode attempt itself.
  const char *version = kdu_get_core_version();
  if (version == nullptr) {
    return 0;
  }
  const char *digits = version;
  if (*digits == 'v' || *digits == 'V') {
    ++digits;
  }
  int major = std::atoi(digits);
  return major >= 8 ? 1 : 0;
}

extern "C" void dcmnorm_kakadu_free_buffer(uint8_t *buffer, size_t) {
  std::free(buffer);
}

extern "C" void dcmnorm_kakadu_free_error(char *error_message) {
  std::free(error_message);
}
