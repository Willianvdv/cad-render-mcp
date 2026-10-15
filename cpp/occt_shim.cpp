// Thin C ABI over Open CASCADE Technology (OCCT): read STEP/IGES/BREP into parts, tessellate them,
// extract render buffers (triangles, B-rep edges, silhouette candidates) and answer geometry queries.
// OCCT is linked dynamically (LGPL-2.1 + OCCT exception); this file only uses its public headers.

#include "occt_shim.h"

#include <BRepAdaptor_Curve.hxx>
#include <BRepPrimAPI_MakeBox.hxx>
#include <BRepPrimAPI_MakeCylinder.hxx>
#include <BRepAlgoAPI_Cut.hxx>
#include <STEPCAFControl_Writer.hxx>
#include <gp_Ax2.hxx>
#include <BRepAdaptor_Surface.hxx>
#include <BRepAlgoAPI_Common.hxx>
#include <BRepBndLib.hxx>
#include <BRepClass3d_SolidClassifier.hxx>
#include <BRepExtrema_DistShapeShape.hxx>
#include <BRepGProp.hxx>
#include <BRepLib.hxx>
#include <BRepLib_ToolTriangulatedShape.hxx>
#include <BRepMesh_IncrementalMesh.hxx>
#include <BRepTools.hxx>
#include <BRep_Builder.hxx>
#include <BRep_Tool.hxx>
#include <Bnd_Box.hxx>
#include <GCPnts_TangentialDeflection.hxx>
#include <GCPnts_UniformAbscissa.hxx>
#include <GProp_GProps.hxx>
#include <IGESCAFControl_Reader.hxx>
#include <Interface_Static.hxx>
#include <Message.hxx>
#include <Message_Messenger.hxx>
#include <Message_PrinterOStream.hxx>
#include <Poly_Polygon3D.hxx>
#include <Poly_PolygonOnTriangulation.hxx>
#include <Poly_Triangulation.hxx>
#include <Quantity_Color.hxx>
#include <STEPCAFControl_Reader.hxx>
#include <Standard_Failure.hxx>
#include <TDF_ChildIterator.hxx>
#include <TDF_LabelSequence.hxx>
#include <TDataStd_Name.hxx>
#include <TDocStd_Document.hxx>
#include <TopExp.hxx>
#include <TopExp_Explorer.hxx>
#include <TopTools_IndexedDataMapOfShapeListOfShape.hxx>
#include <TopTools_IndexedMapOfShape.hxx>
#include <TopTools_ListOfShape.hxx>
#include <Standard_Version.hxx>
#include <TopoDS.hxx>
#include <TopoDS_Compound.hxx>
#include <TopoDS_Edge.hxx>
#include <TopoDS_Face.hxx>
#include <XCAFApp_Application.hxx>
#include <XCAFDoc_ColorTool.hxx>
#include <XCAFDoc_DocumentTool.hxx>
#include <XCAFDoc_ShapeTool.hxx>

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <map>
#include <mutex>
#include <sstream>
#include <string>
#include <unordered_map>
#include <vector>

namespace {

struct Part {
  std::string name;
  std::string path;  // assembly path, '/'-separated
  TopoDS_Shape shape;
  bool has_color = false;
  float color[3] = {0, 0, 0};
  // render buffers (filled by ss_doc_mesh)
  std::vector<float> pos, nrm;     // xyz per vertex
  std::vector<uint32_t> idx;       // triangles
  std::vector<float> edges;        // segments: x0 y0 z0 x1 y1 z1
  std::vector<uint8_t> edge_smooth;  // per segment: 1 = tangent-continuous (smooth) edge
  std::vector<float> sil;          // silhouette candidates: p0 p1 n1 n2 (12 floats)
};

struct Doc {
  std::vector<Part> parts;
};

void set_err(char* err, size_t len, const std::string& msg) {
  if (err && len) {
    std::snprintf(err, len, "%s", msg.c_str());
  }
}

std::string json_escape(const std::string& s) {
  std::string o;
  o.reserve(s.size() + 2);
  for (unsigned char c : s) {
    switch (c) {
      case '"': o += "\\\""; break;
      case '\\': o += "\\\\"; break;
      case '\n': o += "\\n"; break;
      case '\t': o += "\\t"; break;
      default:
        if (c < 0x20) {
          char b[8];
          std::snprintf(b, sizeof b, "\\u%04x", c);
          o += b;
        } else {
          o += (char)c;
        }
    }
  }
  return o;
}

char* dup_str(const std::string& s) {
  char* p = (char*)std::malloc(s.size() + 1);
  std::memcpy(p, s.c_str(), s.size() + 1);
  return p;
}

void quiet_occt() {
  static std::once_flag once;
  std::call_once(once, [] {
    // OCCT prints progress/trace to std::cout by default; stdout is our MCP channel.
    const Handle(Message_Messenger)& m = Message::DefaultMessenger();
    m->RemovePrinters(STANDARD_TYPE(Message_PrinterOStream));
  });
}

std::string label_name(const TDF_Label& l) {
  Handle(TDataStd_Name) n;
  if (!l.IsNull() && l.FindAttribute(TDataStd_Name::GetID(), n)) {
    TCollection_AsciiString a(n->Get());  // UTF-8
    return a.ToCString();
  }
  return "";
}

bool label_color(const Handle(XCAFDoc_ColorTool)& ct, const TDF_Label& l, float out[3]) {
  if (l.IsNull()) return false;
  Quantity_Color c;
  if (ct->GetColor(l, XCAFDoc_ColorSurf, c) || ct->GetColor(l, XCAFDoc_ColorGen, c)) {
    double r, g, b;
    c.Values(r, g, b, Quantity_TOC_sRGB);
    out[0] = (float)r; out[1] = (float)g; out[2] = (float)b;
    return true;
  }
  return false;
}

void collect(const Handle(XCAFDoc_ShapeTool)& st, const Handle(XCAFDoc_ColorTool)& ct, const TDF_Label& label,
             const TopLoc_Location& parent_loc, const std::string& parent_path, int depth, bool split,
             std::vector<Part>& out) {
  TDF_Label ref = label;
  TopLoc_Location loc = parent_loc;
  if (XCAFDoc_ShapeTool::IsReference(label)) {
    XCAFDoc_ShapeTool::GetReferredShape(label, ref);
    loc = parent_loc * XCAFDoc_ShapeTool::GetLocation(label);
  }
  std::string name = label_name(label);
  if (name.empty() || name.rfind("=>", 0) == 0) name = label_name(ref);
  std::string path = parent_path.empty() ? name : parent_path + "/" + name;
  if (XCAFDoc_ShapeTool::IsAssembly(ref) && depth < 64) {
    TDF_LabelSequence comps;
    XCAFDoc_ShapeTool::GetComponents(ref, comps);
    for (int i = 1; i <= comps.Length(); i++) collect(st, ct, comps.Value(i), loc, path, depth + 1, split, out);
    return;
  }
  TopoDS_Shape s = XCAFDoc_ShapeTool::GetShape(ref);
  if (s.IsNull()) return;
  Part p;
  p.name = name.empty() ? "part" : name;
  p.path = path;
  p.shape = loc.IsIdentity() ? s : s.Moved(loc);
  p.has_color = label_color(ct, label, p.color) || label_color(ct, ref, p.color);
  if (!p.has_color) {
    TDF_LabelSequence subs;
    XCAFDoc_ShapeTool::GetSubShapes(ref, subs);
    for (int i = 1; i <= subs.Length() && !p.has_color; i++) p.has_color = label_color(ct, subs.Value(i), p.color);
  }
  if (split) {
    std::vector<TopoDS_Shape> solids;
    for (TopExp_Explorer e(p.shape, TopAbs_SOLID); e.More(); e.Next()) solids.push_back(e.Current());
    if (solids.size() > 1) {
      for (size_t k = 0; k < solids.size(); k++) {
        Part q;
        q.name = p.name + "#" + std::to_string(k + 1);
        q.path = p.path + "#" + std::to_string(k + 1);
        q.shape = solids[k];
        q.has_color = p.has_color;
        std::memcpy(q.color, p.color, sizeof q.color);
        out.push_back(std::move(q));
      }
      return;
    }
  }
  out.push_back(std::move(p));
}

bool load_xcaf(const char* path, bool iges, bool split, std::vector<Part>& parts, std::string& err) {
  Handle(XCAFApp_Application) app = XCAFApp_Application::GetApplication();
  Handle(TDocStd_Document) doc;
  app->NewDocument("MDTV-XCAF", doc);
  bool ok = false;
  if (iges) {
    IGESCAFControl_Reader r;
    r.SetColorMode(true);
    r.SetNameMode(true);
    if (r.ReadFile(path) != IFSelect_RetDone) { err = "cannot read IGES file"; return false; }
    ok = r.Transfer(doc);
  } else {
    STEPCAFControl_Reader r;
    r.SetColorMode(true);
    r.SetNameMode(true);
    r.SetLayerMode(false);
    if (r.ReadFile(path) != IFSelect_RetDone) { err = "cannot read STEP file"; return false; }
    ok = r.Transfer(doc);
  }
  if (!ok) { err = "transfer to XCAF document failed"; return false; }
  Handle(XCAFDoc_ShapeTool) st = XCAFDoc_DocumentTool::ShapeTool(doc->Main());
  Handle(XCAFDoc_ColorTool) ct = XCAFDoc_DocumentTool::ColorTool(doc->Main());
  TDF_LabelSequence roots;
  st->GetFreeShapes(roots);
  for (int i = 1; i <= roots.Length(); i++) collect(st, ct, roots.Value(i), TopLoc_Location(), "", 0, split, parts);
  app->Close(doc);
  return true;
}

bool load_brep(const char* path, std::vector<Part>& parts, std::string& err) {
  TopoDS_Shape s;
  BRep_Builder b;
  if (!BRepTools::Read(s, path, b) || s.IsNull()) { err = "cannot read BREP file"; return false; }
  // One part per solid when the file is a compound of solids, else the whole shape.
  std::vector<TopoDS_Shape> solids;
  for (TopExp_Explorer e(s, TopAbs_SOLID); e.More(); e.Next()) solids.push_back(e.Current());
  if (solids.size() > 1 && s.ShapeType() == TopAbs_COMPOUND) {
    int k = 0;
    for (auto& so : solids) {
      Part p;
      p.name = "solid" + std::to_string(++k);
      p.path = p.name;
      p.shape = so;
      parts.push_back(std::move(p));
    }
  } else {
    Part p;
    p.name = "shape";
    p.path = p.name;
    p.shape = s;
    parts.push_back(std::move(p));
  }
  return true;
}

bool ends_with_ci(const std::string& s, const char* suf) {
  size_t n = std::strlen(suf);
  if (s.size() < n) return false;
  for (size_t i = 0; i < n; i++)
    if (std::tolower((unsigned char)s[s.size() - n + i]) != suf[i]) return false;
  return true;
}

inline void push3(std::vector<float>& v, const gp_XYZ& p) {
  v.push_back((float)p.X()); v.push_back((float)p.Y()); v.push_back((float)p.Z());
}

void mesh_part(Part& p) {
  p.pos.clear(); p.nrm.clear(); p.idx.clear(); p.edges.clear(); p.edge_smooth.clear(); p.sil.clear();
  // Faces -> triangles (+ silhouette candidates on curved faces).
  for (TopExp_Explorer ex(p.shape, TopAbs_FACE); ex.More(); ex.Next()) {
    const TopoDS_Face& f = TopoDS::Face(ex.Current());
    TopLoc_Location loc;
    Handle(Poly_Triangulation) tri = BRep_Tool::Triangulation(f, loc);
    if (tri.IsNull() || tri->NbTriangles() == 0) continue;
    if (!tri->HasNormals()) BRepLib_ToolTriangulatedShape::ComputeNormals(f, tri);
    const gp_Trsf T = loc.Transformation();
    const bool rev = f.Orientation() == TopAbs_REVERSED;
    const uint32_t base = (uint32_t)(p.pos.size() / 3);
    const int nn = tri->NbNodes();
    std::vector<gp_XYZ> pts(nn);
    for (int i = 1; i <= nn; i++) {
      gp_Pnt q = tri->Node(i).Transformed(T);
      pts[i - 1] = q.XYZ();
      push3(p.pos, q.XYZ());
      gp_Dir n = tri->Normal(i).Transformed(T);
      if (rev) n.Reverse();
      push3(p.nrm, n.XYZ());
    }
    const int nt = tri->NbTriangles();
    bool curved = true;
    {
      BRepAdaptor_Surface sa(f, false);
      curved = sa.GetType() != GeomAbs_Plane;
    }
    std::unordered_map<uint64_t, int> half;  // edge (a<b) -> first triangle
    std::vector<gp_XYZ> fn;
    if (curved) { half.reserve(nt * 2); fn.resize(nt); }
    for (int t = 1; t <= nt; t++) {
      int a, b, c;
      tri->Triangle(t).Get(a, b, c);
      if (rev) std::swap(b, c);
      p.idx.push_back(base + a - 1); p.idx.push_back(base + b - 1); p.idx.push_back(base + c - 1);
      if (!curved) continue;
      gp_XYZ n = (pts[b - 1] - pts[a - 1]).Crossed(pts[c - 1] - pts[a - 1]);
      double m = n.Modulus();
      fn[t - 1] = m > 0 ? n / m : n;
      int vs[3] = {a, b, c};
      for (int k = 0; k < 3; k++) {
        uint32_t u = vs[k], v = vs[(k + 1) % 3];
        uint64_t key = ((uint64_t)std::min(u, v) << 32) | std::max(u, v);
        auto it = half.find(key);
        if (it == half.end()) {
          half.emplace(key, t - 1);
        } else if (it->second >= 0) {
          const gp_XYZ& n1 = fn[it->second];
          const gp_XYZ& n2 = fn[t - 1];
          push3(p.sil, pts[u - 1]); push3(p.sil, pts[v - 1]);
          push3(p.sil, n1); push3(p.sil, n2);
          it->second = -1;
        }
      }
    }
  }
  // B-rep edges -> polylines (skip seams and degenerated edges).
  TopTools_IndexedDataMapOfShapeListOfShape e2f;
  TopExp::MapShapesAndAncestors(p.shape, TopAbs_EDGE, TopAbs_FACE, e2f);
  for (int i = 1; i <= e2f.Extent(); i++) {
    const TopoDS_Edge& e = TopoDS::Edge(e2f.FindKey(i));
    if (BRep_Tool::Degenerated(e)) continue;
    const TopTools_ListOfShape& faces = e2f.FindFromIndex(i);
    std::vector<gp_XYZ> line;
    bool seam = false, smooth = false;
    if (!faces.IsEmpty()) {
      const TopoDS_Face& f = TopoDS::Face(faces.First());
      if (BRep_Tool::IsClosed(e, f)) seam = true;
      if (faces.Extent() >= 2) {
        const TopoDS_Face& f2 = TopoDS::Face(faces.Last());
        if (BRep_Tool::HasContinuity(e, f, f2) && BRep_Tool::Continuity(e, f, f2) >= GeomAbs_G1) smooth = true;
      }
      if (seam) continue;
      TopLoc_Location loc;
      Handle(Poly_Triangulation) tri = BRep_Tool::Triangulation(f, loc);
      if (!tri.IsNull()) {
        Handle(Poly_PolygonOnTriangulation) poly = BRep_Tool::PolygonOnTriangulation(e, tri, loc);
        if (!poly.IsNull()) {
          const gp_Trsf T = loc.Transformation();
          const TColStd_Array1OfInteger& nodes = poly->Nodes();
          for (int k = nodes.Lower(); k <= nodes.Upper(); k++) line.push_back(tri->Node(nodes(k)).Transformed(T).XYZ());
        }
      }
    }
    if (line.empty()) {
      TopLoc_Location loc;
      Handle(Poly_Polygon3D) p3 = BRep_Tool::Polygon3D(e, loc);
      if (!p3.IsNull()) {
        const gp_Trsf T = loc.Transformation();
        const TColgp_Array1OfPnt& nodes = p3->Nodes();
        for (int k = nodes.Lower(); k <= nodes.Upper(); k++) line.push_back(nodes(k).Transformed(T).XYZ());
      } else {
        try {
          BRepAdaptor_Curve c(e);
          GCPnts_TangentialDeflection d(c, 0.2, 0.5);
          for (int k = 1; k <= d.NbPoints(); k++) line.push_back(d.Value(k).XYZ());
        } catch (...) {
        }
      }
    }
    for (size_t k = 1; k < line.size(); k++) {
      push3(p.edges, line[k - 1]); push3(p.edges, line[k]);
      p.edge_smooth.push_back(smooth ? 1 : 0);
    }
  }
}

Bnd_Box part_box(const TopoDS_Shape& s) {
  Bnd_Box b;
  BRepBndLib::Add(s, b, true);
  return b;
}

void box_json(std::ostringstream& o, const Bnd_Box& b) {
  if (b.IsVoid()) { o << "null"; return; }
  double x0, y0, z0, x1, y1, z1;
  b.Get(x0, y0, z0, x1, y1, z1);
  o << "[" << x0 << "," << y0 << "," << z0 << "," << x1 << "," << y1 << "," << z1 << "]";
}

// Optional rigid placement: 3x4 row-major matrix (rotation | translation), nullptr = identity.
TopoDS_Shape placed(const TopoDS_Shape& s, const double* m) {
  if (!m) return s;
  gp_Trsf t;
  t.SetValues(m[0], m[1], m[2], m[3], m[4], m[5], m[6], m[7], m[8], m[9], m[10], m[11]);
  return s.Moved(TopLoc_Location(t));
}

}  // namespace

extern "C" {

ss_doc* ss_doc_load(const char* path, bool split_solids, char* err, size_t errlen) {
  quiet_occt();
  try {
    auto* d = new Doc();
    std::string p(path), e;
    bool ok;
    if (ends_with_ci(p, ".brep") || ends_with_ci(p, ".brp")) ok = load_brep(path, d->parts, e);
    else if (ends_with_ci(p, ".igs") || ends_with_ci(p, ".iges")) ok = load_xcaf(path, true, split_solids, d->parts, e);
    else ok = load_xcaf(path, false, split_solids, d->parts, e);
    if (!ok) { set_err(err, errlen, e); delete d; return nullptr; }
    return reinterpret_cast<ss_doc*>(d);
  } catch (const Standard_Failure& f) {
    set_err(err, errlen, std::string("OCCT: ") + f.GetMessageString());
  } catch (const std::exception& x) {
    set_err(err, errlen, x.what());
  } catch (...) {
    set_err(err, errlen, "unknown OCCT exception");
  }
  return nullptr;
}

void ss_doc_free(ss_doc* d) { delete reinterpret_cast<Doc*>(d); }

int ss_doc_part_count(const ss_doc* d) { return (int)reinterpret_cast<const Doc*>(d)->parts.size(); }

int ss_doc_mesh(ss_doc* dd, double lin_defl, double ang_defl, char* err, size_t errlen) {
  auto* d = reinterpret_cast<Doc*>(dd);
  try {
    TopoDS_Compound all;
    BRep_Builder b;
    b.MakeCompound(all);
    for (auto& p : d->parts) b.Add(all, p.shape);
    if (lin_defl <= 0) {
      Bnd_Box box = part_box(all);
      double diag = box.IsVoid() ? 100.0 : std::sqrt(box.SquareExtent());
      lin_defl = std::max(diag * 4e-4, 1e-3);
    }
    BRepMesh_IncrementalMesh mesh(all, lin_defl, false, ang_defl > 0 ? ang_defl : 0.3, true);
    BRepLib::EncodeRegularity(all, 1e-3);
    for (auto& p : d->parts) mesh_part(p);
    return 0;
  } catch (const Standard_Failure& f) {
    set_err(err, errlen, std::string("OCCT mesh: ") + f.GetMessageString());
  } catch (...) {
    set_err(err, errlen, "unknown exception while meshing");
  }
  return -1;
}

ss_part_view ss_doc_part(const ss_doc* dd, int i) {
  const auto& p = reinterpret_cast<const Doc*>(dd)->parts.at(i);
  ss_part_view v{};
  v.name = p.name.c_str();
  v.path = p.path.c_str();
  v.has_color = p.has_color;
  std::memcpy(v.color, p.color, sizeof v.color);
  v.pos = p.pos.data(); v.nrm = p.nrm.data(); v.nverts = p.pos.size() / 3;
  v.idx = p.idx.data(); v.nidx = p.idx.size();
  v.edges = p.edges.data(); v.edge_smooth = p.edge_smooth.data(); v.nedges = p.edge_smooth.size();
  v.sil = p.sil.data(); v.nsil = p.sil.size() / 12;
  return v;
}

void ss_doc_release_mesh(ss_doc* dd) {
  for (auto& p : reinterpret_cast<Doc*>(dd)->parts) {
    std::vector<float>().swap(p.pos); std::vector<float>().swap(p.nrm); std::vector<uint32_t>().swap(p.idx);
    std::vector<float>().swap(p.edges); std::vector<uint8_t>().swap(p.edge_smooth); std::vector<float>().swap(p.sil);
  }
}

char* ss_part_inspect(const ss_doc* dd, int i) {
  try {
    const auto& p = reinterpret_cast<const Doc*>(dd)->parts.at(i);
    std::ostringstream o;
    o.precision(10);
    GProp_GProps vp, sp;
    BRepGProp::VolumeProperties(p.shape, vp);
    BRepGProp::SurfaceProperties(p.shape, sp);
    int nsolid = 0, nshell = 0, nface = 0, nedge = 0;
    for (TopExp_Explorer e(p.shape, TopAbs_SOLID); e.More(); e.Next()) nsolid++;
    for (TopExp_Explorer e(p.shape, TopAbs_SHELL); e.More(); e.Next()) nshell++;
    TopTools_IndexedMapOfShape fm, em;
    TopExp::MapShapes(p.shape, TopAbs_FACE, fm);
    TopExp::MapShapes(p.shape, TopAbs_EDGE, em);
    nface = fm.Extent(); nedge = em.Extent();
    std::map<std::string, int> types;
    std::map<long long, std::pair<int, double>> radii;  // radius in um -> (face count, area)
    std::map<long long, int> tori;
    for (int k = 1; k <= fm.Extent(); k++) {
      BRepAdaptor_Surface s(TopoDS::Face(fm(k)), false);
      const char* t = "other";
      switch (s.GetType()) {
        case GeomAbs_Plane: t = "plane"; break;
        case GeomAbs_Cylinder: {
          t = "cylinder";
          long long r = std::llround(s.Cylinder().Radius() * 1000);
          GProp_GProps fp;
          BRepGProp::SurfaceProperties(fm(k), fp);
          auto& e = radii[r];
          e.first++; e.second += fp.Mass();
          break;
        }
        case GeomAbs_Cone: t = "cone"; break;
        case GeomAbs_Sphere: t = "sphere"; break;
        case GeomAbs_Torus: {
          t = "torus";
          tori[std::llround(s.Torus().MinorRadius() * 1000)]++;
          break;
        }
        case GeomAbs_BezierSurface: t = "bezier"; break;
        case GeomAbs_BSplineSurface: t = "bspline"; break;
        case GeomAbs_SurfaceOfRevolution: t = "revolution"; break;
        case GeomAbs_SurfaceOfExtrusion: t = "extrusion"; break;
        case GeomAbs_OffsetSurface: t = "offset"; break;
        default: break;
      }
      types[t]++;
    }
    Bnd_Box box = part_box(p.shape);
    gp_Pnt c = vp.CentreOfMass();
    o << "{\"name\":\"" << json_escape(p.name) << "\",\"path\":\"" << json_escape(p.path) << "\"";
    o << ",\"volume_mm3\":" << vp.Mass() << ",\"area_mm2\":" << sp.Mass();
    o << ",\"centroid\":[" << c.X() << "," << c.Y() << "," << c.Z() << "],\"bbox\":";
    box_json(o, box);
    o << ",\"solids\":" << nsolid << ",\"shells\":" << nshell << ",\"faces\":" << nface << ",\"edges\":" << nedge;
    o << ",\"face_types\":{";
    bool first = true;
    for (auto& kv : types) { o << (first ? "" : ",") << "\"" << kv.first << "\":" << kv.second; first = false; }
    o << "},\"cylinder_radii\":[";
    first = true;
    for (auto& kv : radii) {
      o << (first ? "" : ",") << "{\"r\":" << kv.first / 1000.0 << ",\"faces\":" << kv.second.first
        << ",\"area_mm2\":" << kv.second.second << "}";
      first = false;
    }
    o << "],\"torus_minor_radii\":[";
    first = true;
    for (auto& kv : tori) {
      o << (first ? "" : ",") << "{\"r\":" << kv.first / 1000.0 << ",\"faces\":" << kv.second << "}";
      first = false;
    }
    o << "]}";
    return dup_str(o.str());
  } catch (const Standard_Failure& f) {
    return dup_str(std::string("{\"error\":\"") + json_escape(f.GetMessageString()) + "\"}");
  } catch (...) {
    return dup_str("{\"error\":\"inspect failed\"}");
  }
}

int ss_part_bbox(const ss_doc* dd, int i, double out[6]) {
  try {
    const auto& p = reinterpret_cast<const Doc*>(dd)->parts.at(i);
    Bnd_Box b = part_box(p.shape);
    if (b.IsVoid()) return -1;
    b.Get(out[0], out[1], out[2], out[3], out[4], out[5]);
    return 0;
  } catch (...) {
    return -1;
  }
}

int ss_distance(const ss_doc* da, int ia, const double* ma, const ss_doc* db, int ib, const double* mb, double out[7]) {
  try {
    const auto& a = reinterpret_cast<const Doc*>(da)->parts.at(ia);
    const auto& b = reinterpret_cast<const Doc*>(db)->parts.at(ib);
    BRepExtrema_DistShapeShape d;
    d.SetMultiThread(true);
    d.LoadS1(placed(a.shape, ma));
    d.LoadS2(placed(b.shape, mb));
    d.Perform();
    if (!d.IsDone() || d.NbSolution() < 1) return -1;
    gp_Pnt p1 = d.PointOnShape1(1), p2 = d.PointOnShape2(1);
    out[0] = d.Value();
    out[1] = p1.X(); out[2] = p1.Y(); out[3] = p1.Z();
    out[4] = p2.X(); out[5] = p2.Y(); out[6] = p2.Z();
    return 0;
  } catch (...) {
    return -1;
  }
}

}  // extern "C"

namespace {
// One boolean common; returns false on error. vol = |volume| of the result.
bool common_once(const TopoDS_Shape& sa, const TopoDS_Shape& sb, double fuzzy, double& vol, Bnd_Box& box) {
  BRepAlgoAPI_Common op;
  TopTools_ListOfShape args, tools;
  args.Append(sa);
  tools.Append(sb);
  op.SetArguments(args);
  op.SetTools(tools);
  op.SetNonDestructive(true);  // never modify the loaded shapes (queries may run concurrently)
  op.SetFuzzyValue(fuzzy);     // near-coincident faces (parts resting on each other) break exact booleans
  op.Build();
  if (op.HasErrors()) return false;
  GProp_GProps vp;
  BRepGProp::VolumeProperties(op.Shape(), vp);
  vol = std::fabs(vp.Mass());
  box.SetVoid();
  BRepBndLib::Add(op.Shape(), box, false);
  return true;
}
}  // namespace

extern "C" {

// Boolean common volume with a retry ladder: exact booleans occasionally return an empty result
// (no error) for grazing/near-coincident configurations; retrying with swapped arguments or a
// larger fuzzy tolerance recovers most of them.
int ss_common(const ss_doc* da, int ia, const double* ma, const ss_doc* db, int ib, const double* mb, double out[7]) {
  try {
    const auto& a = reinterpret_cast<const Doc*>(da)->parts.at(ia);
    const auto& b = reinterpret_cast<const Doc*>(db)->parts.at(ib);
    const TopoDS_Shape sa = placed(a.shape, ma), sb = placed(b.shape, mb);
    struct Try { bool swap; double fuzzy; };
    const Try tries[] = {{false, 1e-3}, {true, 1e-3}, {false, 1e-2}, {true, 5e-2}};
    bool any_ok = false;
    double vol = 0;
    Bnd_Box box;
    for (const Try& t : tries) {
      double v = 0;
      Bnd_Box bx;
      bool ok = t.swap ? common_once(sb, sa, t.fuzzy, v, bx) : common_once(sa, sb, t.fuzzy, v, bx);
      if (!ok) continue;
      any_ok = true;
      if (v > vol) { vol = v; box = bx; }
      if (v > 1e-6) break;
    }
    if (!any_ok) return -1;
    out[0] = vol;
    if (box.IsVoid()) {
      for (int k = 1; k < 7; k++) out[k] = 0;
    } else {
      box.Get(out[1], out[2], out[3], out[4], out[5], out[6]);
    }
    return 0;
  } catch (...) {
    return -1;
  }
}

// Count tessellation nodes of a (placed) that lie strictly inside solid b (placed), sampling at
// most max_pts nodes inside b's box. Robust fallback when a boolean reports no overlap.
int ss_points_inside(const ss_doc* da, int ia, const double* ma, const ss_doc* db, int ib, const double* mb, int max_pts) {
  try {
    const auto& a = reinterpret_cast<const Doc*>(da)->parts.at(ia);
    const auto& b = reinterpret_cast<const Doc*>(db)->parts.at(ib);
    TopoDS_Shape sa = placed(a.shape, ma), sb = placed(b.shape, mb);
    Bnd_Box bb;
    BRepBndLib::Add(sb, bb, true);
    std::vector<gp_Pnt> pts;
    for (TopExp_Explorer ex(sa, TopAbs_FACE); ex.More(); ex.Next()) {
      TopLoc_Location loc;
      Handle(Poly_Triangulation) tri = BRep_Tool::Triangulation(TopoDS::Face(ex.Current()), loc);
      if (tri.IsNull()) continue;
      const gp_Trsf T = loc.Transformation();
      for (int i = 1; i <= tri->NbNodes(); i++) {
        gp_Pnt q = tri->Node(i).Transformed(T);
        if (!bb.IsOut(q)) pts.push_back(q);
      }
    }
    // Edge points every ~2 mm: coarse face meshes can straddle a thin part without a node inside it.
    TopTools_IndexedMapOfShape edges;
    TopExp::MapShapes(sa, TopAbs_EDGE, edges);
    for (int i = 1; i <= edges.Extent(); i++) {
      const TopoDS_Edge& e = TopoDS::Edge(edges(i));
      if (BRep_Tool::Degenerated(e)) continue;
      try {
        BRepAdaptor_Curve c(e);
        GCPnts_UniformAbscissa ua(c, 2.0);
        if (!ua.IsDone()) continue;
        for (int k = 1; k <= ua.NbPoints(); k++) {
          gp_Pnt q = c.Value(ua.Parameter(k));
          if (!bb.IsOut(q)) pts.push_back(q);
        }
      } catch (...) {
      }
    }
    if (pts.empty()) return 0;
    size_t stride = std::max<size_t>(1, pts.size() / std::max(1, max_pts));
    BRepClass3d_SolidClassifier cls(sb);
    int inside = 0;
    for (size_t k = 0; k < pts.size(); k += stride) {
      cls.Perform(pts[k], 0.05);
      if (cls.State() == TopAbs_IN) inside++;
    }
    return inside;
  } catch (...) {
    return -1;
  }
}

void ss_str_free(char* s) { std::free(s); }

const char* ss_occt_version(void) { return OCC_VERSION_COMPLETE; }

// Write a small, named and coloured demo assembly (a miniature roof rack) as STEP: used as the
// public test fixture and for trying the tools without your own CAD files. Units: mm, Z up,
// front = +Y.
int ss_write_demo_step(const char* path, char* err, size_t errlen) {
  quiet_occt();
  try {
    auto box = [](double x0, double y0, double z0, double x1, double y1, double z1) {
      return BRepPrimAPI_MakeBox(gp_Pnt(x0, y0, z0), gp_Pnt(x1, y1, z1)).Shape();
    };
    auto cyl = [](gp_Pnt base, gp_Dir dir, double r, double h) {
      return BRepPrimAPI_MakeCylinder(gp_Ax2(base, dir), r, h).Shape();
    };
    auto plate = [&](double x0, double x1) {
      TopoDS_Shape p = box(x0, 0, 0, x1, 800, 120);
      for (double y : {100.0, 300.0, 500.0, 700.0})  // R6 bolt holes through the plate
        p = BRepAlgoAPI_Cut(p, cyl(gp_Pnt(x0 - 1, y, 60), gp_Dir(1, 0, 0), 6.0, (x1 - x0) + 2)).Shape();
      return p;
    };
    Handle(XCAFApp_Application) app = XCAFApp_Application::GetApplication();
    Handle(TDocStd_Document) doc;
    app->NewDocument("MDTV-XCAF", doc);
    Handle(XCAFDoc_ShapeTool) st = XCAFDoc_DocumentTool::ShapeTool(doc->Main());
    Handle(XCAFDoc_ColorTool) ct = XCAFDoc_DocumentTool::ColorTool(doc->Main());
    TDF_Label asm_label = st->NewShape();
    TDataStd_Name::Set(asm_label, "DEMO_RACK");
    auto add = [&](const TopoDS_Shape& s, const char* name, double r, double g, double b) {
      TDF_Label part = st->AddShape(s, false);
      TDataStd_Name::Set(part, name);
      ct->SetColor(part, Quantity_Color(r, g, b, Quantity_TOC_sRGB), XCAFDoc_ColorSurf);
      TDF_Label comp = st->AddComponent(asm_label, part, TopLoc_Location());
      TDataStd_Name::Set(comp, name);
    };
    const double alu[3] = {0.91, 0.92, 0.93};
    add(plate(-405, -400), "PLATE_L", alu[0], alu[1], alu[2]);
    add(plate(400, 405), "PLATE_R", alu[0], alu[1], alu[2]);
    const char* bars[] = {"BAR_1", "BAR_2", "BAR_3", "BAR_4"};
    int k = 0;
    for (double y : {80.0, 280.0, 480.0, 680.0})  // 40 x 40 bars, 160 mm apart, touching both plates
      add(box(-400, y, 120, 400, y + 40, 160), bars[k++], alu[0], alu[1], alu[2]);
    TopoDS_Shape wheel = BRepAlgoAPI_Cut(cyl(gp_Pnt(0, 400, 160), gp_Dir(0, 0, 1), 250, 150),
                                         cyl(gp_Pnt(0, 400, 159), gp_Dir(0, 0, 1), 100, 152)).Shape();
    add(wheel, "WHEEL", 0.08, 0.08, 0.08);  // rests on the bars (z = 160)
    add(cyl(gp_Pnt(0, 805, 90), gp_Dir(0, 1, 0), 45, 60), "LAMP", 0.05, 0.05, 0.06);
    add(cyl(gp_Pnt(-420, 300, 60), gp_Dir(1, 0, 0), 8, 40), "BOLT", 0.7, 0.72, 0.74);  // M16 through R6 hole: clashes
    st->UpdateAssemblies();
    STEPCAFControl_Writer w;
    w.SetColorMode(true);
    w.SetNameMode(true);
    if (!w.Transfer(doc, STEPControl_AsIs)) { set_err(err, errlen, "STEP transfer failed"); return -1; }
    if (w.Write(path) != IFSelect_RetDone) { set_err(err, errlen, "STEP write failed"); return -1; }
    app->Close(doc);
    return 0;
  } catch (const Standard_Failure& f) {
    set_err(err, errlen, std::string("OCCT: ") + f.GetMessageString());
  } catch (...) {
    set_err(err, errlen, "unknown exception writing demo STEP");
  }
  return -1;
}

}  // extern "C"
