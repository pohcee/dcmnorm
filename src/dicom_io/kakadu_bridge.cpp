#include <algorithm>
#include <cassert>
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
#include "kdu_stripe_compressor.h"
#include "kdu_stripe_decompressor.h"

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
  int get_capabilities() override {
    return KDU_TARGET_CAP_SEQUENTIAL;
  }

  bool write(const kdu_byte *buf, int num_bytes) override {
    if (num_bytes < 0 || (num_bytes > 0 && buf == nullptr)) {
      return false;
    }
    if (num_bytes == 0) {
      return true;
    }

    const size_t count = static_cast<size_t>(num_bytes);
    if (rewrite_active_) {
      if (rewrite_limit_ < count || rewrite_pos_ + count > bytes_.size()) {
        return false;
      }
      std::memcpy(bytes_.data() + rewrite_pos_, buf, count);
      rewrite_pos_ += count;
      rewrite_limit_ -= count;
      return true;
    }

    bytes_.insert(bytes_.end(), buf, buf + count);
    return true;
  }

  bool start_rewrite(kdu_long backtrack) override {
    if (rewrite_active_ || backtrack < 0) {
      return false;
    }
    const size_t bt = static_cast<size_t>(backtrack);
    if (bt > bytes_.size()) {
      return false;
    }
    rewrite_active_ = true;
    rewrite_pos_ = bytes_.size() - bt;
    rewrite_limit_ = bt;
    return true;
  }

  bool end_rewrite() override {
    if (!rewrite_active_) {
      return false;
    }
    rewrite_active_ = false;
    rewrite_pos_ = 0;
    rewrite_limit_ = 0;
    return true;
  }

  const std::vector<uint8_t> &bytes() const {
    return bytes_;
  }

private:
  std::vector<uint8_t> bytes_;
  bool rewrite_active_ = false;
  size_t rewrite_pos_ = 0;
  size_t rewrite_limit_ = 0;
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
  return std::vector<int>(static_cast<size_t>(count), value);
}

std::vector<int> make_sample_offsets(int samples_per_pixel) {
  std::vector<int> offsets(static_cast<size_t>(samples_per_pixel));
  for (int i = 0; i < samples_per_pixel; ++i) {
    offsets[static_cast<size_t>(i)] = i;
  }
  return offsets;
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
  auto sample_offsets = make_sample_offsets(samples_per_pixel);
  auto sample_gaps = make_component_array(samples_per_pixel, samples_per_pixel);
  auto row_gaps = make_component_array(cols * samples_per_pixel, samples_per_pixel);
  auto precisions = make_component_array(bits_stored, samples_per_pixel);

  std::vector<uint8_t> result;
  if (bits_stored <= 8) {
    result.resize(static_cast<size_t>(rows) * static_cast<size_t>(cols) * static_cast<size_t>(samples_per_pixel));
    decompressor.pull_stripe(result.data(), stripe_heights.data(), sample_offsets.data(), sample_gaps.data(), row_gaps.data(), precisions.data(), nullptr, 0);
  } else {
    std::vector<kdu_int16> buffer(static_cast<size_t>(rows) * static_cast<size_t>(cols) * static_cast<size_t>(samples_per_pixel));
    std::unique_ptr<bool[]> signed_flags(new bool[static_cast<size_t>(samples_per_pixel)]);
    for (int i = 0; i < samples_per_pixel; ++i) {
      signed_flags[static_cast<size_t>(i)] = (is_signed != 0);
    }
    decompressor.pull_stripe(buffer.data(), stripe_heights.data(), sample_offsets.data(), sample_gaps.data(), row_gaps.data(), precisions.data(), signed_flags.get(), nullptr, 0);
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
    const uint8_t *pixels,
    size_t pixels_len,
    int rows,
    int cols,
    int samples_per_pixel,
    int bits_stored,
    int is_signed,
    int reversible) {
  validate_common_args(rows, cols, samples_per_pixel, bits_stored);
  if (pixels == nullptr && pixels_len != 0) {
    throw std::runtime_error("null pixel pointer");
  }

  const size_t bytes_per_sample = (bits_stored <= 8) ? 1u : 2u;
  const size_t expected_len = static_cast<size_t>(rows) * static_cast<size_t>(cols) * static_cast<size_t>(samples_per_pixel) * bytes_per_sample;
  if (pixels_len != expected_len) {
    throw std::runtime_error("pixel buffer length does not match image metadata");
  }

  memory_target output;

  siz_params siz;
  siz.set(Scomponents, 0, 0, samples_per_pixel);
  siz.set(Creversible, 0, 0, (reversible != 0));
  siz.set(Cycc, 0, 0, false);
  for (int c = 0; c < samples_per_pixel; ++c) {
    siz.set(Sdims, c, 0, rows);
    siz.set(Sdims, c, 1, cols);
    siz.set(Sprecision, c, 0, bits_stored);
    siz.set(Ssigned, c, 0, (is_signed != 0));
  }
  siz.finalize_all();

  kdu_codestream codestream_obj;
  codestream_obj.create(&siz, &output);
  codestream_obj.access_siz()->finalize_all();

  kdu_stripe_compressor compressor;
  compressor.start(codestream_obj);

  auto stripe_heights = make_component_array(rows, samples_per_pixel);
  auto sample_offsets = make_sample_offsets(samples_per_pixel);
  auto sample_gaps = make_component_array(samples_per_pixel, samples_per_pixel);
  auto row_gaps = make_component_array(cols * samples_per_pixel, samples_per_pixel);
  auto precisions = make_component_array(bits_stored, samples_per_pixel);

  if (bits_stored <= 8) {
    std::vector<kdu_byte> buffer(pixels, pixels + pixels_len);
    compressor.push_stripe(buffer.data(), stripe_heights.data(), sample_offsets.data(), sample_gaps.data(), row_gaps.data(), precisions.data(), 0);
  } else {
    std::vector<kdu_int16> buffer(expected_len / 2);
    for (size_t i = 0; i < buffer.size(); ++i) {
      const uint16_t word = static_cast<uint16_t>(pixels[i * 2]) |
                            (static_cast<uint16_t>(pixels[i * 2 + 1]) << 8);
      buffer[i] = static_cast<kdu_int16>(word);
    }
    std::unique_ptr<bool[]> signed_flags(new bool[static_cast<size_t>(samples_per_pixel)]);
    for (int i = 0; i < samples_per_pixel; ++i) {
      signed_flags[static_cast<size_t>(i)] = (is_signed != 0);
    }
    compressor.push_stripe(buffer.data(), stripe_heights.data(), sample_offsets.data(), sample_gaps.data(), row_gaps.data(), precisions.data(), signed_flags.get(), 0);
  }

  if (!compressor.finish()) {
    throw std::runtime_error("Kakadu compressor did not finish successfully");
  }

  codestream_obj.destroy();
  return output.bytes();
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
    const uint8_t *pixels,
    size_t pixels_len,
    int rows,
    int cols,
    int samples_per_pixel,
    int bits_stored,
    int is_signed,
    int reversible,
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
    std::vector<uint8_t> encoded = encode_impl(pixels, pixels_len, rows, cols, samples_per_pixel, bits_stored, is_signed, reversible);
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

extern "C" void dcmnorm_kakadu_free_buffer(uint8_t *buffer, size_t) {
  std::free(buffer);
}

extern "C" void dcmnorm_kakadu_free_error(char *error_message) {
  std::free(error_message);
}
