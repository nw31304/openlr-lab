// Decode result used only while the onboarding tour is showing the
// Results/Trace panels -- not fetched from any tile server at tour time (no
// dependency on whichever tileset is actually loaded), but it IS a real
// OpenLR v3 string (`CwRtXyUYzyORBABB/y4jfTUK`, a residential route through
// a roundabout near Zutphenseweg/Schoolstraat, Netherlands). Cross-checked
// against a real decode of this exact string run through this app against
// real tile data -- segment count (8), per-segment lengths/FRC/FOW, the
// covered range (segment 0 fully bypassed by the positive offset), and the
// pos/neg offset values below all match that real decode. `stable_id`
// follows the app's real convention: `<source-way-id>-<split-index>`, since
// a single source way gets split into multiple graph segments wherever
// another way attaches at an interior point (Invariant 1) -- e.g.
// `6821064-1`/`6821064-0` are two different graph segments carved out of
// the same OSM way. That matters twice over:
//   1. The *basemap* underneath (Liberty/Bright/Positron/OSM/Carto -- see
//      BASEMAPS in Map.jsx) is always OSM-derived regardless of which
//      OpenLR road pmtiles the user has loaded, so baking in the real
//      street's own vertices makes the sample path hug the real road shown
//      underneath instead of cutting across it.
//   2. A v3 string's bearing/DNP/offset fields are bucketed ranges, not
//      single values -- bearing_lb/ub here are a genuine 11.25° bucket
//      (191.25/202.5, not equal), dnp_lb/ub a genuine ~58.6 m bucket
//      (234.375/292.969), and the offsets are fractions of the routed leg
//      length. Carrying the real, unequal [lb, ub] pairs through the sample
//      is deliberate: the whole app represents these as bounded intervals
//      end to end (see coverage_range in openlr-engine/src/wkt.rs), never
//      collapsed to a bucket midpoint, and the sample data should model
//      that rather than quietly contradict it with a fake lb == ub.
export const TOUR_SAMPLE_OPENLR_STRING = 'CwRtXyUYzyORBABB/y4jfTUK';

export const TOUR_SAMPLE_DECODE_RESULT = {
  ok: true,
  format: 'TomTomV3',
  location_type: 'Line',
  wkt: 'LINESTRING (6.2256437 52.1670919, 6.2255673 52.1668211, 6.2254926 52.1665432, 6.2254840 52.1665109, 6.2254581 52.1664270, 6.2254500 52.1663887, 6.2254500 52.1663554, 6.2254486 52.1662598, 6.2254270 52.1662505, 6.2254078 52.1662393, 6.2253914 52.1662266, 6.2253781 52.1662125, 6.2253682 52.1661975, 6.2253619 52.1661817, 6.2253593 52.1661655, 6.2253605 52.1661493, 6.2253654 52.1661334, 6.2253740 52.1661180, 6.2253861 52.1661036, 6.2254014 52.1660903, 6.2254196 52.1660785, 6.2254403 52.1660684, 6.2254632 52.1660603, 6.2254878 52.1660542, 6.2255135 52.1660503, 6.2255398 52.1660488, 6.2255663 52.1660495, 6.2255923 52.1660525, 6.2256173 52.1660578, 6.2256409 52.1660652, 6.2256625 52.1660746, 6.2256817 52.1660857, 6.2256953 52.1660950, 6.2258199 52.1660569, 6.2258607 52.1660383, 6.2258871 52.1660182, 6.2259471 52.1659691, 6.2259796 52.1659425, 6.2260319 52.1658886, 6.2260603 52.1658593, 6.2262380 52.1656514, 6.2263542 52.1655223)',
  segments: [
    {
      frc: 4, fow: 3, direction: 'Both', length_m: 48.7,
      stable_id: '6821064-1', tile: '12/2118/1349', local_index: 0, segment_id: 900001,
      geometry: [[6.2258002, 52.1676211], [6.2256714, 52.1671902]],
    },
    {
      frc: 4, fow: 3, direction: 'Both', length_m: 76.6,
      stable_id: '6821064-0', tile: '12/2118/1349', local_index: 1, segment_id: 900002,
      geometry: [[6.2256714, 52.1671902], [6.2255673, 52.1668211], [6.2254926, 52.1665432],
                 [6.2254840, 52.1665109]],
    },
    {
      frc: 4, fow: 3, direction: 'Both', length_m: 28.1,
      stable_id: '1087908192-0', tile: '12/2118/1349', local_index: 2, segment_id: 900003,
      geometry: [[6.2254840, 52.1665109], [6.2254581, 52.1664270], [6.2254500, 52.1663887],
                 [6.2254500, 52.1663554], [6.2254486, 52.1662598]],
    },
    {
      frc: 4, fow: 4, direction: 'Both', length_m: 21.7,
      stable_id: '1087908194-0', tile: '12/2118/1349', local_index: 3, segment_id: 900004,
      geometry: [[6.2254486, 52.1662598], [6.2254270, 52.1662505], [6.2254078, 52.1662393],
                 [6.2253914, 52.1662266], [6.2253781, 52.1662125], [6.2253682, 52.1661975],
                 [6.2253619, 52.1661817], [6.2253593, 52.1661655], [6.2253605, 52.1661493],
                 [6.2253654, 52.1661334], [6.2253740, 52.1661180], [6.2253861, 52.1661036],
                 [6.2254014, 52.1660903]],
    },
    {
      frc: 4, fow: 4, direction: 'Both', length_m: 9.0,
      stable_id: '1087908197-0', tile: '12/2118/1349', local_index: 4, segment_id: 900005,
      geometry: [[6.2254014, 52.1660903], [6.2254196, 52.1660785], [6.2254403, 52.1660684],
                 [6.2254632, 52.1660603], [6.2254878, 52.1660542], [6.2255135, 52.1660503]],
    },
    {
      frc: 4, fow: 4, direction: 'Both', length_m: 14.0,
      stable_id: '1087908198-0', tile: '12/2118/1349', local_index: 5, segment_id: 900006,
      geometry: [[6.2255135, 52.1660503], [6.2255398, 52.1660488], [6.2255663, 52.1660495],
                 [6.2255923, 52.1660525], [6.2256173, 52.1660578], [6.2256409, 52.1660652],
                 [6.2256625, 52.1660746], [6.2256817, 52.1660857], [6.2256953, 52.1660950]],
    },
    {
      frc: 4, fow: 3, direction: 'Both', length_m: 26.4,
      stable_id: '336956461-0', tile: '12/2118/1349', local_index: 6, segment_id: 900007,
      geometry: [[6.2256953, 52.1660950], [6.2258199, 52.1660569], [6.2258607, 52.1660383],
                 [6.2258871, 52.1660182], [6.2259471, 52.1659691], [6.2259796, 52.1659425]],
    },
    {
      frc: 4, fow: 3, direction: 'Both', length_m: 64.6,
      stable_id: '6821048-2', tile: '12/2118/1349', local_index: 7, segment_id: 900008,
      geometry: [[6.2259796, 52.1659425], [6.2260319, 52.1658886], [6.2260603, 52.1658593],
                 [6.2262380, 52.1656514], [6.2264342, 52.1654333]],
    },
  ],
  lrps: [
    {
      lon: 6.2257826, lat: 52.1675169, frc: 4, fow: 3, lfrcnp: 4,
      bearing_lb: 191.25, bearing_ub: 202.5, dnp_lb: 234.375, dnp_ub: 292.969,
    },
    {
      lon: 6.2264326, lat: 52.1654169, frc: 4, fow: 3, lfrcnp: null,
      bearing_lb: 326.25, bearing_ub: 337.5, dnp_lb: null, dnp_ub: null,
    },
  ],
  pos_offset_lb: 59.8, pos_offset_ub: 61.0,
  neg_offset_lb: 11.3, neg_offset_ub: 12.4,
  covered_start_idx: 1, covered_end_idx: 7,
  covered_pos_offset_lb: 11.1, covered_pos_offset_ub: 12.3,
  covered_neg_offset_lb: 11.3, covered_neg_offset_ub: 12.4,
  offsets_approximate: false,
};
