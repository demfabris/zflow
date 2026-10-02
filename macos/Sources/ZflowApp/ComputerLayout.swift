import SwiftUI

/// One computer on the arrangement canvas. The canvas draws only placed
/// tiles, at the spot the layout gives them.
struct LayoutTile: Identifiable, Equatable {
  var computer: Computer
  /// A paired computer's state; nil for this Mac.
  var state: Peer.State?
  /// Its key's mark.
  var mark: String?
  var placed = true
  var id: String { computer.id }

  /// The layout's computers, each paired one with its state and mark.
  static func tiles(_ layout: Layout, peers: [Peer], ownMark: String? = nil) -> [LayoutTile] {
    layout.monitors.map { computer in
      guard let name = computer.peer else {
        return LayoutTile(computer: computer, state: nil, mark: ownMark)
      }
      let peer = peers.first { $0.name == name }
      return LayoutTile(computer: computer, state: peer?.state, mark: peer?.mark)
    }
  }
}

/// Where the shelf of found computers sits on the canvas.
enum ShelfEdge {
  case bottom
  case trailing
}

struct ComputerLayout: View {
  var tiles: [LayoutTile]
  /// Found computers, on a shelf until one is dragged onto the board.
  var unplaced: [Unplaced] = []
  var shelf = ShelfEdge.bottom
  /// Shows the shelf while it is empty, saying more may show up.
  var searching = false
  var move: (Computer, Int, Int, Int) -> Void
  /// A shelf computer dropped with its tile's corner at a layout point.
  var place: (Unplaced, Int, Int, Int) -> Void = { _, _, _, _ in }
  @State private var dragging: String?
  @State private var translation = CGSize.zero
  /// The shelf computer being carried, and where the pointer is.
  @State private var carried: String?
  @State private var carriedAt = CGPoint.zero

  /// The size the engine gives a new computer's tile until it says its own.
  nonisolated static let newTile = CGSize(width: 1920, height: 1080)
  private static let space = "canvas"

  private var placed: [LayoutTile] { tiles.filter(\.placed) }
  private var showsShelf: Bool { !unplaced.isEmpty || searching }

  var body: some View {
    GeometryReader { area in
      let shelfFrame = shelfRect(area.size)
      let board = boardRect(area.size, shelf: shelfFrame)
      let bounds = layoutBounds
      let scale = min(
        (board.width - 90) / max(bounds.width, 1),
        (board.height - 70) / max(bounds.height, 1), 0.12)
      let origin = CGPoint(
        x: board.midX - bounds.width * scale / 2 - bounds.minX * scale,
        y: board.midY - bounds.height * scale / 2 - bounds.minY * scale)
      ZStack {
        RoundedRectangle(cornerRadius: 16).fill(.quinary)
        dots
        if placed.isEmpty {
          Text("Detecting this Mac’s displays…").foregroundStyle(.secondary)
            .position(x: board.midX, y: board.midY)
        }
        ForEach(placed) { tile in
          accessibleTile(tile, scale: scale, origin: origin)
        }
        if let shelfFrame {
          shelfView(scale: scale, origin: origin, board: board, frame: shelfFrame)
            .frame(width: shelfFrame.width, height: shelfFrame.height)
            .position(x: shelfFrame.midX, y: shelfFrame.midY)
        }
        if let computer = unplaced.first(where: { $0.id == carried }) {
          if board.contains(carriedAt) && !(shelfFrame?.contains(carriedAt) ?? false) {
            // Where it lands, at the board's scale.
            RoundedRectangle(cornerRadius: 10)
              .strokeBorder(Color.accentColor, style: StrokeStyle(lineWidth: 1.5, dash: [5, 4]))
              .frame(width: Self.newTile.width * scale, height: Self.newTile.height * scale)
              .position(carriedAt)
          }
          ShelfTile(computer: computer, carried: true)
            .position(carriedAt)
            .allowsHitTesting(false)
        }
      }
      .coordinateSpace(.named(Self.space))
      .clipShape(.rect(cornerRadius: 16))
    }
  }

  /// A dotted backdrop, like a desk mat.
  private var dots: some View {
    Canvas { context, size in
      let spacing: CGFloat = 18
      var path = Path()
      for x in stride(from: spacing / 2, to: size.width, by: spacing) {
        for y in stride(from: spacing / 2, to: size.height, by: spacing) {
          path.addEllipse(in: CGRect(x: x - 0.75, y: y - 0.75, width: 1.5, height: 1.5))
        }
      }
      context.fill(path, with: .color(.secondary.opacity(0.3)))
    }
    .accessibilityHidden(true)
  }

  private func shelfRect(_ size: CGSize) -> CGRect? {
    guard showsShelf else { return nil }
    switch shelf {
    case .bottom: return CGRect(x: 14, y: size.height - 122, width: size.width - 28, height: 108)
    case .trailing: return CGRect(x: size.width - 272, y: 22, width: 250, height: size.height - 44)
    }
  }

  /// The part of the canvas the arrangement fills.
  private func boardRect(_ size: CGSize, shelf frame: CGRect?) -> CGRect {
    let all = CGRect(origin: .zero, size: size)
    guard let frame else { return all }
    switch shelf {
    case .bottom: return CGRect(x: 0, y: 0, width: size.width, height: frame.minY)
    case .trailing: return CGRect(x: 0, y: 0, width: frame.minX, height: size.height)
    }
  }

  private func shelfView(scale: CGFloat, origin: CGPoint, board: CGRect, frame: CGRect)
    -> some View
  {
    let found = ForEach(unplaced) { computer in
      shelfTile(computer, scale: scale, origin: origin, board: board, shelf: frame)
    }
    let title = Text("Found on your network").font(.callout.weight(.semibold))
    let looking = HStack(spacing: 8) {
      ProgressView().controlSize(.small)
      Text("Looking for computers…").foregroundStyle(.secondary)
    }
    .font(.callout)
    return Group {
      switch shelf {
      case .bottom:
        HStack(spacing: 18) {
          VStack(alignment: .leading, spacing: 3) {
            title
            Text("Drag one next to a screen to add it. Nothing else to do.")
              .font(.callout).foregroundStyle(.secondary)
          }
          .frame(maxWidth: .infinity, alignment: .leading)
          if unplaced.isEmpty { looking }
          found
        }
        .padding(.horizontal, 14)
      case .trailing:
        VStack(alignment: .leading, spacing: 10) {
          title.foregroundStyle(.secondary)
          if unplaced.isEmpty { looking }
          found
          Spacer(minLength: 0)
          if unplaced.contains(where: \.placeable) {
            Text("Drag it next to This Mac.").font(.caption).foregroundStyle(.secondary)
          }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(12)
      }
    }
    .frame(maxWidth: .infinity, maxHeight: .infinity)
    .background(.background.opacity(0.4), in: .rect(cornerRadius: 12))
    .overlay {
      RoundedRectangle(cornerRadius: 12)
        .strokeBorder(
          Color.secondary.opacity(0.4), style: StrokeStyle(lineWidth: 1.5, dash: [6, 4]))
    }
  }

  private func shelfTile(
    _ computer: Unplaced, scale: CGFloat, origin: CGPoint, board: CGRect, shelf: CGRect
  ) -> some View {
    let local = placed.first { $0.computer.peer == nil }?.computer
    return ShelfTile(computer: computer, carried: false)
      .frame(maxWidth: self.shelf == .trailing ? .infinity : 150)
      .opacity(carried == computer.id ? 0.35 : 1)
      .gesture(
        DragGesture(minimumDistance: 2, coordinateSpace: .named(Self.space))
          .onChanged { value in
            carried = computer.id
            carriedAt = value.location
          }
          .onEnded { value in
            carried = nil
            let at = value.location
            guard board.contains(at), !shelf.contains(at) else { return }
            let (x, y) = Self.corner(at: at, scale: scale, origin: origin)
            place(computer, x, y, Int((CGFloat(14) / scale).rounded()))
          },
        isEnabled: computer.placeable
      )
      .accessibilityElement(children: .ignore)
      .accessibilityLabel(Text(computer.name))
      .accessibilityValue(Text(computer.detail))
      .accessibilityAction(named: Text("Place beside This Mac")) {
        guard computer.placeable else { return }
        let x = local.map { $0.x + $0.width } ?? 0
        place(computer, x, local?.y ?? 0, 150)
      }
  }

  /// The layout point of the corner of a new tile centered on `point`.
  nonisolated static func corner(at point: CGPoint, scale: CGFloat, origin: CGPoint) -> (Int, Int) {
    let x = (point.x - origin.x) / scale - newTile.width / 2
    let y = (point.y - origin.y) / scale - newTile.height / 2
    return (Int(x.rounded()), Int(y.rounded()))
  }

  private func positionedTile(_ tile: LayoutTile, scale: CGFloat, origin: CGPoint) -> some View {
    let computer = tile.computer
    let active: Bool = dragging == computer.id
    let offset: CGSize = active ? translation : .zero
    let midpointX: CGFloat = CGFloat(computer.x) + CGFloat(computer.width) / 2
    let midpointY: CGFloat = CGFloat(computer.y) + CGFloat(computer.height) / 2
    let center = CGPoint(
      x: origin.x + midpointX * scale + offset.width,
      y: origin.y + midpointY * scale + offset.height)
    return tileView(tile, scale: scale, active: active)
      .position(center)
      .zIndex(active ? 1 : 0)
  }

  private func accessibleTile(_ tile: LayoutTile, scale: CGFloat, origin: CGPoint) -> some View {
    let computer = tile.computer
    return positionedTile(tile, scale: scale, origin: origin)
      .gesture(dragGesture(for: computer, scale: scale))
      .accessibilityElement(children: .ignore)
      .accessibilityLabel(Text(computer.label))
      .accessibilityValue(
        Text("\(tile.state?.label ?? "This Mac"), position \(computer.x), \(computer.y)")
      )
      .accessibilityAction(named: Text("Move left")) {
        move(computer, computer.x - 100, computer.y, 150)
      }
      .accessibilityAction(named: Text("Move right")) {
        move(computer, computer.x + 100, computer.y, 150)
      }
      .accessibilityAction(named: Text("Move up")) {
        move(computer, computer.x, computer.y - 100, 150)
      }
      .accessibilityAction(named: Text("Move down")) {
        move(computer, computer.x, computer.y + 100, 150)
      }
  }

  private func dragGesture(for computer: Computer, scale: CGFloat) -> some Gesture {
    DragGesture(minimumDistance: 2)
      .onChanged { (value: DragGesture.Value) in
        dragging = computer.id
        translation = value.translation
      }
      .onEnded { (value: DragGesture.Value) in
        let deltaX: Int = Int((value.translation.width / scale).rounded())
        let deltaY: Int = Int((value.translation.height / scale).rounded())
        let tolerance: Int = Int((CGFloat(14) / scale).rounded())
        move(computer, computer.x + deltaX, computer.y + deltaY, tolerance)
        dragging = nil
        translation = .zero
      }
  }

  /// This Mac has an accent outline. A paired computer that is not
  /// connected is drawn faded, with a dashed outline.
  private func tileView(_ tile: LayoutTile, scale: CGFloat, active: Bool) -> some View {
    let computer = tile.computer
    let local = computer.peer == nil
    let away = !local && [nil, .paired, .unreachable].contains(tile.state)
    let shape = RoundedRectangle(cornerRadius: 10)
    return VStack(alignment: .leading, spacing: 2) {
      HStack(alignment: .top) {
        Image(systemName: local ? "laptopcomputer" : "desktopcomputer")
          .font(.title3)
          .foregroundStyle(local ? AnyShapeStyle(.tint) : AnyShapeStyle(.secondary))
        Spacer(minLength: 4)
        if let mark = tile.mark { KeyMark(mark: mark).opacity(away ? 0.6 : 1) }
      }
      Spacer(minLength: 4)
      Text(computer.label).font(.callout.weight(.semibold)).lineLimit(1)
      if let state = tile.state {
        HStack(spacing: 5) {
          StateDot(state: state)
          Text(state.label).lineLimit(1)
        }
        .font(.caption).foregroundStyle(.secondary)
      } else if local && computer.label != "This Mac" {
        Text("This Mac").font(.caption).foregroundStyle(.secondary)
      }
    }
    .padding(10)
    .frame(
      width: max(110, CGFloat(computer.width) * scale),
      height: max(76, CGFloat(computer.height) * scale),
      alignment: .topLeading
    )
    .background(Color(nsColor: .controlBackgroundColor).opacity(away ? 0.55 : 1), in: shape)
    .overlay {
      if active || local {
        shape.strokeBorder(Color.accentColor, lineWidth: 2)
      } else if away {
        shape.strokeBorder(
          Color.secondary.opacity(0.5), style: StrokeStyle(lineWidth: 1.5, dash: [5, 4]))
      } else {
        shape.strokeBorder(Color.secondary.opacity(0.35), lineWidth: 1)
      }
    }
    .shadow(
      color: .black.opacity(active ? 0.13 : 0.05), radius: active ? 8 : 3, y: active ? 4 : 1
    )
  }

  private var layoutBounds: CGRect {
    let bounds = placed.reduce(CGRect.null) {
      $0.union(
        CGRect(
          x: $1.computer.x, y: $1.computer.y, width: $1.computer.width, height: $1.computer.height))
    }
    return bounds.isNull ? CGRect(x: 0, y: 0, width: 1600, height: 1000) : bounds
  }
}

/// A found computer on the shelf. One that cannot be placed yet says why.
private struct ShelfTile: View {
  var computer: Unplaced
  var carried: Bool

  var body: some View {
    VStack(alignment: .leading, spacing: 1) {
      HStack(alignment: .top) {
        Image(systemName: computer.symbol).font(.body).foregroundStyle(.secondary)
        Spacer(minLength: 4)
        if let mark = computer.mark {
          KeyMark(mark: mark)
        } else if computer.state == .identifying {
          ProgressView().controlSize(.mini)
        }
      }
      Spacer(minLength: 4)
      Text(computer.name).font(.callout.weight(.semibold)).lineLimit(1)
      Text(computer.detail).font(.caption).foregroundStyle(.secondary).lineLimit(1)
    }
    .padding(.vertical, 10).padding(.horizontal, 12)
    .frame(width: carried ? 150 : nil, height: 80, alignment: .topLeading)
    .frame(minWidth: 130)
    .background(Color(nsColor: .controlBackgroundColor), in: .rect(cornerRadius: 12))
    .overlay { RoundedRectangle(cornerRadius: 12).strokeBorder(Color.secondary.opacity(0.35)) }
    .opacity(computer.placeable ? 1 : 0.6)
    .shadow(color: .black.opacity(carried ? 0.18 : 0.08), radius: carried ? 10 : 6, y: 3)
    .rotationEffect(.degrees(carried ? -2 : 0))
  }
}

extension Unplaced {
  var symbol: String { os == .macos ? "laptopcomputer" : "desktopcomputer" }
  /// The line under its name on the shelf.
  var detail: String {
    switch state {
    case .identifying: "Identifying…"
    case .differentVersion: "Different zflow version"
    case .duplicateName: "Same name as another"
    case .ready:
      if trustsYou { "Added this Mac" } else { os.map { $0 == .macos ? "macOS" : "Linux" } ?? "" }
    }
  }
}

/// A key's mark: four small squares, so two computers with one name look
/// different. Each of the mark's first four hex digits picks one of eight
/// colors, as the GNOME window draws it too.
struct KeyMark: View {
  var mark: String

  nonisolated static let palette: [Color] = [
    0xE5484D, 0xF76B15, 0xFFC53D, 0x30A46C, 0x12A594, 0x0090FF, 0x8E4EC6, 0xD6409F,
  ].map { rgb in
    Color(
      red: Double(rgb >> 16 & 0xFF) / 255, green: Double(rgb >> 8 & 0xFF) / 255,
      blue: Double(rgb & 0xFF) / 255)
  }
  /// The palette index of each square.
  nonisolated static func indices(_ mark: String) -> [Int] {
    mark.prefix(4).compactMap(\.hexDigitValue).map { $0 & 7 }
  }

  var body: some View {
    HStack(spacing: 2) {
      ForEach(Array(Self.indices(mark).enumerated()), id: \.offset) { _, index in
        RoundedRectangle(cornerRadius: 2).fill(Self.palette[index]).frame(width: 7, height: 7)
      }
    }
    .accessibilityHidden(true)
  }
}
