// C ABI between cad-render-mcp (Rust) and Open CASCADE Technology.
#pragma once
#include <stddef.h>
#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct ss_doc ss_doc;

typedef struct {
  const char* name;
  const char* path;
  bool has_color;
  float color[3];          // sRGB 0..1
  const float* pos;        // nverts * 3
  const float* nrm;        // nverts * 3
  size_t nverts;
  const uint32_t* idx;     // nidx (triangles)
  size_t nidx;
  const float* edges;      // nedges * 6 (segment endpoints)
  const uint8_t* edge_smooth;  // nedges
  size_t nedges;
  const float* sil;        // nsil * 12 (p0, p1, n1, n2)
  size_t nsil;
} ss_part_view;

// split_solids: a leaf shape holding several solids becomes one part per solid ("name#k").
ss_doc* ss_doc_load(const char* path, bool split_solids, char* err, size_t errlen);
void ss_doc_free(ss_doc* d);
int ss_doc_part_count(const ss_doc* d);
int ss_doc_mesh(ss_doc* d, double lin_defl, double ang_defl, char* err, size_t errlen);
ss_part_view ss_doc_part(const ss_doc* d, int i);
void ss_doc_release_mesh(ss_doc* d);
char* ss_part_inspect(const ss_doc* d, int i);
int ss_part_bbox(const ss_doc* d, int i, double out[6]);
// ma/mb: optional rigid placement, 3x4 row-major (rotation | translation); NULL = as loaded.
int ss_distance(const ss_doc* a, int ia, const double* ma, const ss_doc* b, int ib, const double* mb, double out[7]);
int ss_common(const ss_doc* a, int ia, const double* ma, const ss_doc* b, int ib, const double* mb, double out[7]);
int ss_points_inside(const ss_doc* a, int ia, const double* ma, const ss_doc* b, int ib, const double* mb, int max_pts);
void ss_str_free(char* s);
const char* ss_occt_version(void);
int ss_write_demo_step(const char* path, char* err, size_t errlen);

#ifdef __cplusplus
}
#endif
